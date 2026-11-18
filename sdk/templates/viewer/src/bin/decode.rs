//! The decode worker (atrium.toml's `[[workers]]` "decode"). It runs with
//! no capabilities, gets the opened file's descriptor and answers with a
//! `Picture`; atrium-worker does the framing, the checks and the limits.

atrium_worker::main!({{crate}}::workers::decode);
