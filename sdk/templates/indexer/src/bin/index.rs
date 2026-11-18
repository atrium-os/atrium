//! The index worker (atrium.toml's `[[workers]]` "index"): one process for
//! the app's run, no capabilities, messages in and out. atrium-worker does
//! the framing, the checks and the limits, and runs it in capability mode:
//! it can open nothing by name, only what it was given.

atrium_worker::main!({{crate}}::workers::index);
