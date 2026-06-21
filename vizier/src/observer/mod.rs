use std::path::PathBuf;

use anyhow::Result;

use crate::observation::{Observation, WakeObservation};

pub mod common;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, Clone)]
pub struct ObserverConfig {
    pub watch_path: Option<PathBuf>,
    pub all_connections: bool,
}

#[derive(Debug, Clone)]
pub struct WakeConfig {
    pub no_public_ip: bool,
}

pub trait Observer {
    fn snapshot(&mut self) -> Result<Observation>;
}

pub trait Waker {
    fn wake(&self) -> Result<WakeObservation>;
}

#[cfg(target_os = "macos")]
pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    macos::create_observer(config)
}

#[cfg(target_os = "linux")]
pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    linux::create_observer(config)
}

#[cfg(target_os = "windows")]
pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    windows::create_observer(config)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    Box::new(common::BaselineObserver::new(config))
}

#[cfg(target_os = "macos")]
pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    macos::create_waker(config)
}

#[cfg(target_os = "linux")]
pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    linux::create_waker(config)
}

#[cfg(target_os = "windows")]
pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    windows::create_waker(config)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    Box::new(common::BaselineWaker::new(config))
}
