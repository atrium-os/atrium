//! The parse worker (atrium.toml's `[[workers]]` "parse"). It runs with no
//! capabilities, gets the response's descriptor and answers with a `Feed`;
//! atrium-worker does the framing, the checks and the limits.

atrium_worker::main!({{crate}}::workers::parse);
