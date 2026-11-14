//! atrium-net's logic, kept free of system calls so it runs in host tests.

pub mod plan;
pub mod wrap;

#[cfg(target_os = "freebsd")]
pub mod sys;
