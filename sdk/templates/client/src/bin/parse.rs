//! The parse worker (atrium.toml's `[[workers]]` "parse"). It runs with no
//! capabilities, gets the response's descriptor and answers with a `Feed`;
//! atrium-worker does the framing, the checks and the limits, and runs it in
//! capability mode: it can open nothing by name, only what it was given.

atrium_worker::main!({{crate}}::workers::parse);
