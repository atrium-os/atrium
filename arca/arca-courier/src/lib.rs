//! Arca's courier (arca.md §5): the only process that sends a bearer
//! secret over the network, and only to the one endpoint in its label.
//!
//! - [`schema`]: the fixed binary request the client sends, and the reply.
//! - [`send`]: one request in §5's order: connect through the fetch rules,
//!   TLS to the bound host, then ask Arca, attach, send, strip
//!   `Set-Cookie` into records, mask echoes, wipe. Redirects come back to
//!   the client and are never followed.
//! - [`vault`]: arcad's courier ops.

pub mod schema;
pub mod send;
pub mod vault;
