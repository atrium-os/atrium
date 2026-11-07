//! Vinculum's transport (docs/spec/bluetooth.md §5): the one process that
//! touches the controller. It frames H4 in both directions, drops anything
//! malformed, and passes the host's commands only if [`allow::allowed`]
//! says so, so that even a compromised host cannot send vendor commands that
//! write controller memory (§5.3).
#![forbid(unsafe_code)]

pub mod allow;
pub mod relay;
