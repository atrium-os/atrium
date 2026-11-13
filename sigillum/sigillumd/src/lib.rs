//! `sigillumd`, Sigillum's broker (sigillum.md §7), as a pure state machine.
//!
//! Every input (a client request with the label the kernel's peer uid
//! resolved to, a press on the sheet, a PIN, a driver record, the clock)
//! goes into [`broker::Broker`], which answers with [`broker::Action`]s for
//! the daemon's I/O loop to carry out. So every rule is a host test, like
//! arcad's `server.rs`. The daemon around it (sockets, the launch registry,
//! frescod, Capsicum) is S2.
#![forbid(unsafe_code)]

pub mod bio;
pub mod broker;
pub mod label;
pub mod rp;
