#![feature(box_syntax)]
#![feature(integer_atomics)]

// Logging
#[macro_use]
extern crate log;
extern crate e2d2;
extern crate env_logger;
extern crate fnv;
extern crate rand;
extern crate toml;
extern crate separator;
#[macro_use]
extern crate serde_derive;
extern crate eui48;
extern crate ipnet;
extern crate serde;
extern crate uuid;
#[macro_use]
extern crate error_chain;

pub mod nftcp;

pub use cmanager::{Connection, L234Data, ReleaseCause, UserData, ConRecord, TcpState};

pub mod errors;
mod cmanager;
mod timer_wheel;

use ipnet::Ipv4Net;
use eui48::MacAddress;
use uuid::Uuid;
use separator::Separatable;

use e2d2::native::zcsi::*;
use e2d2::common::ErrorKind as E2d2ErrorKind;
use e2d2::scheduler::*;
use e2d2::allocators::CacheAligned;
use e2d2::interface::{PortQueue, PmdPort};

use errors::*;
use nftcp::*;

use std::fs::File;
use std::io::Read;
use std::any::Any;
use std::net::Ipv4Addr;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::ptr;
use std::sync::mpsc::Sender;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;
use std::sync::mpsc::RecvTimeoutError;
use std::fmt;
use std::cmp::Ordering;


#[derive(Deserialize)]
struct Config {
    proxyengine: Configuration,
}

#[derive(Deserialize, Clone)]
pub struct Configuration {
    pub targets: Vec<TargetConfig>,
    pub engine: EngineConfig,
    pub test_size: Option<usize>,
}

#[derive(Deserialize, Clone)]
pub struct EngineConfig {
    pub namespace: String,
    pub mac: String,
    pub ipnet: String,
    pub timeouts: Option<Timeouts>,
    pub port: u16,
}

#[derive(Deserialize, Clone)]
pub struct TargetConfig {
    pub id: String,
    pub ip: Ipv4Addr,
    pub mac: Option<MacAddress>,
    pub linux_if: Option<String>,
    pub port: u16,
}

#[derive(Deserialize, Clone)]
pub struct Timeouts {
    established: Option<u64>, // in millis
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        Timeouts { established: Some(200) }
    }
}

impl Timeouts {
    pub fn default_or_some(timeouts: &Option<Timeouts>) -> Timeouts {
        let mut t = Timeouts::default();
        if timeouts.is_some() {
            let timeouts = timeouts.clone().unwrap();
            if timeouts.established.is_some() {
                t.established = timeouts.established;
            }
        }
        t
    }
}

pub fn read_config(filename: &str) -> Result<Configuration> {
    let mut toml_str = String::new();
    let _ = File::open(filename)
        .and_then(|mut f| f.read_to_string(&mut toml_str))
        .chain_err(|| E2d2ErrorKind::ConfigurationError(format!("Could not read file {}", filename)))?;

    info!("toml configuration:\n {}", toml_str);

    let config: Config = match toml::from_str(&toml_str) {
        Ok(value) => value,
        Err(err) => return Err(err.into()),
    };

    match config.proxyengine.engine.ipnet.parse::<Ipv4Net>() {
        Ok(_) => match config.proxyengine.engine.mac.parse::<MacAddress>() {
            Ok(_) => Ok(config.proxyengine),
            Err(e) => Err(e.into()),
        },
        Err(e) => Err(e.into()),
    }
}

struct MyData {
    c2s_count: usize,
    s2c_count: usize,
    avg_latency: f64,
}

impl MyData {
    fn new() -> MyData {
        MyData {
            c2s_count: 0,
            s2c_count: 0,
            avg_latency: 0.0f64,
        }
    }

    fn init(&mut self) {
        self.c2s_count = 0;
        self.s2c_count = 0;
        self.avg_latency = 0.0f64;
    }
}

// using the container makes compiler happy wrt. to static lifetime for the mydata content
pub struct Container {
    mydata: MyData,
}

impl UserData for Container {
    #[inline]
    fn ref_userdata(&self) -> &Any {
        &self.mydata
    }

    fn mut_userdata(&mut self) -> &mut Any {
        &mut self.mydata
    }

    fn init(&mut self) {
        self.mydata.init();
    }
}

impl Container {
    pub fn new() -> Box<Container> {
        Box::new(Container { mydata: MyData::new() })
    }
}

pub fn get_mac_from_ifname(ifname: &str) -> Result<MacAddress> {
    let iface = Path::new("/sys/class/net").join(ifname).join("address");
    let mut macaddr = String::new();
    fs::File::open(iface).map_err(|e| e.into()).and_then(|mut f| {
        f.read_to_string(&mut macaddr)
            .map_err(|e| e.into())
            .and_then(|_| MacAddress::parse_str(&macaddr.lines().next().unwrap_or("")).map_err(|e| e.into()))
    })
}

pub fn get_mac_string_from_ifname(ifname: &str) -> Result<String> {
    let iface = Path::new("/sys/class/net").join(ifname).join("address");
    let mut macaddr = String::new();
    fs::File::open(iface).map_err(|e| e.into()).and_then(|mut f| {
        f.read_to_string(&mut macaddr)
            .map_err(|e| e.into())
            .and_then(|_| Ok(macaddr.lines().next().unwrap_or("").to_string()))
    })
}

pub fn print_hard_statistics(port_id: u16) -> i32 {
    let stats = RteEthStats::new();
    let retval;
    unsafe {
        retval = rte_eth_stats_get(port_id, &stats as *const RteEthStats);
    }
    if retval == 0 {
        println!("Port {}:\n{}\n", port_id, stats);
    }
    retval
}

pub fn print_soft_statistics(port_id: u16) -> i32 {
    let stats = RteEthStats::new();
    let retval;
    unsafe {
        retval = rte_eth_stats_get(port_id, &stats as *const RteEthStats);
    }
    if retval == 0 {
        println!("Port {}:\n{}\n", port_id, stats);
    }
    retval
}

pub fn print_xstatistics(port_id: u16) -> i32 {
    let len;
    unsafe {
        len = rte_eth_xstats_get_names_by_id(port_id, ptr::null(), 0, ptr::null());
        if len < 0 {
            return len;
        }
        let xstats_names = vec![
            RteEthXstatName {
                name: [0; RTE_ETH_XSTATS_NAME_SIZE],
            };
            len as usize
        ];
        let ids = vec![0u64; len as usize];
        if len != rte_eth_xstats_get_names_by_id(port_id, xstats_names.as_ptr(), len as u32, ptr::null()) {
            return -1;
        };
        let values = vec![0u64; len as usize];

        if len != rte_eth_xstats_get_by_id(port_id, ptr::null(), values.as_ptr(), 0 as u32) {
            return -1;
        }

        for i in 0..len as usize {
            rte_eth_xstats_get_id_by_name(port_id, xstats_names[i].to_ptr(), &ids[i]);
            {
                println!("{}, {}: {}", i, xstats_names[i].to_str().unwrap(), values[i]);
            }
        }
    }
    len
}

pub fn setup_pipelines<F1, F2>(
    core: i32,
    ports: HashSet<CacheAligned<PortQueue>>,
    sched: &mut StandaloneScheduler,
    proxy_config: &Configuration,
    f_select_server: Arc<F1>,
    f_process_payload_c_s: Arc<F2>,
    tx: Sender<MessageFrom>,
) where
    F1: Fn(&mut Connection) + Sized + Send + Sync + 'static,
    F2: Fn(&mut Connection, &mut [u8], usize) + Sized + Send + Sync + 'static,
{
    let mut kni: Option<&CacheAligned<PortQueue>> = None;
    let mut pci: Option<&CacheAligned<PortQueue>> = None;
    for port in &ports {
        debug!(
            "setup_pipeline on core {}: port {} --  {} rxq {} txq {}",
            core,
            port.port,
            port.port.mac_address(),
            port.rxq(),
            port.txq(),
        );
        if port.port.is_kni() {
            kni = Some(port);
        } else {
            pci = Some(port);
        }
    }

    if pci.is_none() {
        panic!("need at least one pci port");
    }

    // kni receive queue is served on the first core (i.e. rxq==0)

    if kni.is_none() && is_kni_core(pci.unwrap()) {
        // we need a kni i/f for queue 0
        panic!("need one kni port for queue 0");
    }

    let uuid = Uuid::new_v4();
    let name = String::from("KniHandleRequest");

    if is_kni_core(pci.unwrap()) {
        sched.add_runnable(
            Runnable::from_task(
                uuid,
                name,
                KniHandleRequest {
                    kni_port: kni.unwrap().port.clone(),
                },
            ).ready(), // this task must be ready from the beginning to enable managing the KNI i/f
        );
    }

    setup_forwarder(
        core,
        pci.unwrap(),
        kni.unwrap(),
        sched,
        proxy_config,
        f_select_server,
        f_process_payload_c_s,
        tx,
    );
}

#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct PipelineId {
    pub core: u16,
    pub port_id: u16,
    pub rxq: u16,
}

impl fmt::Display for PipelineId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<c{}, p{}, rx{}>", self.core, self.port_id, self.rxq)
    }
}

#[derive(Debug)]
pub enum TaskType {
    TcpGenerator = 0,
    Pipe2Kni = 1,
    Pipe2Pci = 2,
    NoTaskTypes = 3, // for iteration over TaskType
}

pub enum MessageFrom {
    Channel(PipelineId, Sender<MessageTo>),
    CRecord(PipelineId, ConRecord),
    ClientSyn(PipelineId, ConRecord),
    Established(PipelineId, ConRecord),
    GenTimeStamp(PipelineId, u64, u64, u64),
    // generator timestamps : pipeline, count of sent syn, tsc-value
    StartEngine(Sender<MessageTo>),
    Task(PipelineId, Uuid, TaskType),
    PrintPerformance(Vec<i32>),
    // performance for the cores selected by the indices
    Exit, // exit recv thread
}

pub enum MessageTo {
    Hello,
    ConRecords(Vec<(PipelineId, ConRecord)>),
    Exit, // exit recv thread
}

pub fn spawn_recv_thread(mrx: Receiver<MessageFrom>, mut context: NetBricksContext, configuration: Configuration) {
    /*
        mrx: receiver for messages from all the pipelines running
    */
    let _handle = thread::spawn(move || {
        let mut senders = HashMap::new();
        let mut tasks: Vec<Vec<(PipelineId, Uuid)>> = Vec::with_capacity(TaskType::NoTaskTypes as usize);
        let mut con_records: Vec<(PipelineId, ConRecord)> = Vec::with_capacity(5000);
        let mut reply_to_main = None;

        for _t in 0..TaskType::NoTaskTypes as usize {
            tasks.push(Vec::<(PipelineId, Uuid)>::with_capacity(16));
        }
        context.execute_schedulers();

        // set up kni
        debug!("Number of PMD ports: {}", PmdPort::num_pmd_ports());
        for port in context.ports.values() {
            debug!(
                "port {}:{} -- mac_address= {}",
                port.port_type(),
                port.port_id(),
                port.mac_address()
            );
            if port.is_kni() {
                setup_kni(
                    port.linux_if().unwrap(),
                    &configuration.engine.ipnet,
                    &configuration.engine.mac,
                    &configuration.engine.namespace,
                );
            }
        }

        loop {
            match mrx.recv_timeout(Duration::from_millis(60000)) {
                Ok(MessageFrom::StartEngine(reply_channel)) => {
                    reply_to_main = Some(reply_channel);
                    // distribute message to all pipelines
                    debug!("received StartEngine");
                    /*
                    for t in 0..TaskType::NoTaskTypes as usize {
                        debug!("starting tasks {:?}", t);
                        for (pipeline_id, uuid) in &tasks[t] {
                            let sync_sender = context.scheduler_channels.get(&(pipeline_id.core as i32));
                            if sync_sender.is_some() {
                                sync_sender.unwrap().send(SchedulerCommand::SetTaskState(uuid.clone(), true)).unwrap();
                            }
                        }
                    } */
                    for s in &context.scheduler_channels {
                        s.1.send(SchedulerCommand::SetTaskStateAll(true)).unwrap();
                    }
                }
                Ok(MessageFrom::PrintPerformance(indices)) => {
                    for i in &indices {
                        context
                            .scheduler_channels
                            .get(i)
                            .unwrap()
                            .send(SchedulerCommand::SetTaskStateAll(false))
                            .unwrap();
                        context
                            .scheduler_channels
                            .get(i)
                            .unwrap()
                            .send(SchedulerCommand::GetPerformance)
                            .unwrap();
                    }
                }
                Ok(MessageFrom::Task(pipeline_id, uuid, task_type)) => {
                    debug!("{}: task uuid= {}, type={:?}", pipeline_id, uuid, task_type);
                    tasks[task_type as usize].push((pipeline_id, uuid));
                }
                Ok(MessageFrom::GenTimeStamp(pipeline_id, syn_count, tsc0, tsc1)) => info!(
                    "pipe {}: GenTimeStamp -> count= {}, tsc0= {}, tsc1= {}",
                    pipeline_id,
                    syn_count,
                    tsc0.separated_string(),
                    tsc1.separated_string(),
                ),
                Ok(MessageFrom::Channel(pipeline_id, sender)) => {
                    debug!("got sender from {}", pipeline_id);
                    sender.send(MessageTo::Hello).unwrap();
                    senders.insert(pipeline_id, sender);
                }
                Ok(MessageFrom::Exit) => {
                    debug!("received Exit");
                    print_hard_statistics(1u16);

                    for port in context.ports.values() {
                        println!("Port {}:{}", port.port_type(), port.port_id());
                        port.print_soft_statistics();
                    }
                    println!("Connection Records ...");
                    let cmp = |a: &(PipelineId, ConRecord), b: &(PipelineId, ConRecord)| {
                        let ordering= a.1.client_sock.ip().cmp(b.1.client_sock.ip());
                        if ordering==Ordering::Equal {
                            a.1.client_sock.port().cmp(&b.1.client_sock.port())
                        } else { ordering }
                    };
                    con_records.sort_by(cmp);
                    for (p,con_record) in &con_records {
                        info!(
                            "CRecord: pipe {}, c-sock={}, p_port= {}, hold = {:12} cy, c/s-setup = {:8} cy/{:8} cy, {}, c/s_state = {:?}/{:?}, rc = {:?}",
                            p,
                            con_record.client_sock,
                            con_record.p_port,
                            con_record.con_hold,
                            con_record.c_ack_recv - con_record.c_syn_recv,
                            con_record.s_ack_sent - con_record.s_syn_sent,
                            con_record.server_index,
                            con_record.c_state,
                            con_record.s_state,
                            con_record.get_release_cause(),
                        );
                    }
                    if reply_to_main.is_some() { reply_to_main.unwrap().send(MessageTo::ConRecords(con_records)).unwrap(); }
                    context.stop();
                    /*
                    senders.values().for_each(|ref tx| {
                        tx.send(MessageTo::Exit).unwrap();
                    });
                    // give receivers time to read the Exit message before closing the channel
                    thread::sleep(Duration::from_millis(200 as u64));
                    */
                    break;
                }
                Ok(MessageFrom::CRecord(pipe, con_record)) => {
                    debug!(
                        "CRecord: pipe {}, p_port= {}, hold = {:12} cy, c/s-setup = {:8} cy/{:8} cy, {}, c/s_state = {:?}/{:?}, rc = {:?}",
                        pipe,
                        con_record.p_port,
                        con_record.con_hold,
                        con_record.c_ack_recv - con_record.c_syn_recv,
                        con_record.s_ack_sent - con_record.s_syn_sent,
                        con_record.server_index,
                        con_record.c_state,
                        con_record.s_state,
                        con_record.get_release_cause(),
                    );
                    con_records.push((pipe, con_record));
                }
                Ok(MessageFrom::ClientSyn(pipe, con_record)) => {
                    debug!(
                        "ClientSyn: pipe {}: p_port= {}, c-sock={}",
                        pipe,
                        con_record.p_port,
                        con_record.client_sock,
                    );
                    con_records.push((pipe, con_record));
                }
                Ok(MessageFrom::Established(pipe, con_record)) => {
                    debug!(
                        "Established: pipe {}: p_port= {}, c-sock={},  c/s-setup = {:8} cy/{:8} cy, {} ",
                        pipe,
                        con_record.p_port,
                        con_record.client_sock,
                        con_record.c_ack_recv - con_record.c_syn_recv,
                        con_record.s_ack_sent - con_record.s_syn_sent,
                        con_record.server_index,
                    );
                    con_records.push((pipe, con_record));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(e) => {
                    error!("error receiving from message channel: {}", e);
                    break;
                }
                //_ => warn!("illegal message"),
            }
            match context
                .reply_receiver
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_millis(10))
            {
                Ok(SchedulerReply::PerformanceData(core, map)) => {
                    for d in map {
                        info!(
                            "{:2}: {:20} {:>15} count= {:12}",
                            core,
                            (d.1).0,
                            (d.1).1.separated_string(),
                            (d.1).2.separated_string(),
                        )
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(e) => {
                    error!("error receiving from SchedulerReply channel: {}", e);
                    break;
                }
            }
        }
        info!("exiting recv thread ...");
    });
}
