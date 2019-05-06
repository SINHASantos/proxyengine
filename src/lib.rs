#![feature(box_syntax)]
#![feature(integer_atomics)]
#![feature(trait_alias)]

// Logging
#[macro_use]
extern crate log;
extern crate e2d2;
extern crate env_logger;
extern crate fnv;
extern crate toml;
extern crate separator;
#[macro_use]
extern crate serde_derive;
extern crate eui48;
extern crate ipnet;
extern crate serde;
extern crate uuid;
extern crate netfcts;

mod nftcp;
mod cmanager;

pub use cmanager::{ProxyConnection, Extension, ProxyRecStore};

use netfcts::tasks::TaskType;
use netfcts::tasks::KniHandleRequest;
use eui48::MacAddress;
use uuid::Uuid;

use e2d2::scheduler::*;
use e2d2::interface::PmdPort;

use nftcp::setup_delayed_proxy;
use netfcts::{new_port_queues_for_core, physical_ports_for_core, RunConfiguration};
use netfcts::comm::{MessageFrom, MessageTo, PipelineId};
use netfcts::tcp_common::L234Data;
use netfcts::utils::Timeouts;
use netfcts::recstore::Store64;

use std::net::Ipv4Addr;
use std::collections::{HashMap, };
use std::sync::Arc;

pub trait FnSelectServer = Fn(&mut ProxyConnection) + Sized + Send + Sync + Clone + 'static;
pub trait FnPayload = Fn(&mut ProxyConnection, &mut [u8], usize) + Sized + Send + Sync + Clone + 'static;

#[derive(Deserialize, Clone)]
pub struct Configuration {
    pub targets: Vec<TargetConfig>,
    pub engine: EngineConfig,
    pub test_size: Option<usize>,
}

#[derive(Deserialize, Clone, PartialEq)]
pub enum ProxyMode {
    DelayedV0,
    Delayed,
}

#[derive(Deserialize, Clone)]
pub struct EngineConfig {
    pub timeouts: Option<Timeouts>,
    pub port: u16,
    pub detailed_records: Option<bool>,
    pub mode: Option<ProxyMode>,
}

#[derive(Deserialize, Clone)]
pub struct TargetConfig {
    pub id: String,
    pub ip: Ipv4Addr,
    pub mac: Option<MacAddress>,
    pub linux_if: Option<String>,
    pub port: u16,
}

pub fn setup_pipes_delayed_proxy<F1, F2>(
    core: i32,
    pmd_ports: HashMap<String, Arc<PmdPort>>,
    sched: &mut StandaloneScheduler,
    run_configuration: RunConfiguration<Configuration, Store64<Extension>>,
    servers: Vec<L234Data>,
    f_select_server: F1,
    f_process_payload_c_s: F2,
) where
    F1: FnSelectServer,
    F2: FnPayload,
{
    for pmd_port in physical_ports_for_core(core, &pmd_ports) {
        debug!("setup_pipelines for {} on core {}:", pmd_port.name(), core);
        let mut kni_port = None;
        if pmd_port.kni_name().is_some() {
            kni_port = pmd_ports.get(pmd_port.kni_name().unwrap());
        }
        let (pci, kni) = new_port_queues_for_core(core, &pmd_port, kni_port);
        if pci.is_some() {
            debug!(
                "pmd_port= {}, rxq= {}",
                pci.as_ref().unwrap().port_queue.port,
                pci.as_ref().unwrap().port_queue.rxq()
            );
        } else {
            debug!("pmd_port= None");
        }

        if kni.is_some() {
            debug!(
                "associated kni= {}, rxq= {}",
                kni.as_ref().unwrap().port,
                kni.as_ref().unwrap().rxq()
            );
        } else {
            debug!("associated kni= None");
        }

        let uuid = Uuid::new_v4();
        let name = String::from("KniHandleRequest");

        // Kni request handler runs on first core of the associated pci port (rxq == 0)
        if pci.is_some()
            && kni.is_some()
            && kni.as_ref().unwrap().port.is_native_kni()
            && pci.as_ref().unwrap().port_queue.rxq() == 0
        {
            sched.add_runnable(
                Runnable::from_task(
                    uuid,
                    name,
                    KniHandleRequest {
                        kni_port: kni.as_ref().unwrap().port.clone(),
                        last_tick: 0,
                    },
                )
                .move_ready(), // this task must be ready from the beginning to enable managing the KNI i/f
            );
        }

        if pci.is_some() && kni.is_some() {
            setup_delayed_proxy(
                core,
                pci.unwrap(),
                kni.unwrap(),
                sched,
                run_configuration.clone(),
                servers.clone(),
                f_select_server.clone(),
                f_process_payload_c_s.clone(),
            );
        }
    }
}
