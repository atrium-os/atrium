//! Typed errors. Surfaces both validator and syscall failures with
//! enough detail for portcullisd to log + surface to the user, but
//! without leaking strings that might be useful to an attacker
//! (e.g. nothing from /etc/master.passwd, no internal paths beyond
//! what's already in the policy file).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum JaildError {
    #[error("policy file: {0}")]
    Policy(#[from] jaild_policy::PolicyError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Protocol(#[from] serde_json::Error),

    #[error("policy violation: {rule}: {detail}")]
    PolicyViolation { rule: &'static str, detail: String },

    #[error("syscall {name} failed: errno={errno} ({msg})")]
    Syscall { name: &'static str, errno: i32, msg: String },
}
