//! Host tests for atrium-gpu-amd's pure Rust modules (no kernel KPI): the
//! firmware header parser and the IP discovery reader. The kmod includes the
//! same files by path (../rs), so these tests exercise the code that ships.
#![no_std]

#[path = "../../rs/fw.rs"]
pub mod fw;
#[path = "../../rs/discovery.rs"]
pub mod discovery;
#[path = "../../rs/ipinfo.rs"]
pub mod ipinfo;
#[path = "../../rs/psp_plan.rs"]
pub mod psp_plan;
#[path = "../../rs/ip_fw.rs"]
pub mod ip_fw;
#[path = "../../rs/smu_metrics.rs"]
pub mod smu_metrics;

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
