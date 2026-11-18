//! The index worker (atrium.toml's `[[workers]]` "index"): one process for
//! the app's run, no capabilities, messages in and out. atrium-worker does
//! the framing, the checks and the limits.

atrium_worker::main!({{crate}}::workers::index);
