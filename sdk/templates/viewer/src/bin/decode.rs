//! The decode worker (atrium.toml's `[[workers]]` "decode"). It runs with
//! no capabilities, gets the opened file's descriptor and answers with a
//! `Picture`; atrium-worker does the framing, the checks and the limits,
//! and runs it in capability mode: it can open nothing by name, only what
//! it was given.

atrium_worker::main!({{crate}}::workers::decode);
