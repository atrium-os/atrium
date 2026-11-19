//! `prelum-ipp` (printing.md §6.3, §7.3): talks IPP to one printer for
//! prelumd. [`conn`] is one request over HTTP/1.1 (plain or TLS, the key
//! pinned in [`tls`]); [`worker`] is the jailed process's loop.

#![forbid(unsafe_code)]

mod auth;
pub mod conn;
pub mod tls;
pub mod worker;
