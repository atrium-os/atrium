//! A persona's route domain (vpn.md §5.4, V2): the routing table its apps'
//! jails live in, from atrium-net's root-only control socket.
//!
//! A persona whose policy names a `vpn` gets that tunnel's domain. atrium-net
//! hands out one FIB per domain name; with the tunnel down the table is empty,
//! so the persona's apps reach nothing rather than leaving by the plain
//! network.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const SOCK: &str = "/atrium/sockets/atrium-net/ctl.sock";

/// The FIB for route domain `vpn`, or why there is none.
pub fn fib_for(vpn: &str) -> Result<u32, String> {
    if vpn.is_empty() || vpn.len() > 32 || !vpn.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(format!("bad vpn name {vpn:?}"));
    }
    let path = std::env::var("ATRIUM_NET_SOCK").unwrap_or_else(|_| SOCK.into());
    let mut s = UnixStream::connect(&path).map_err(|e| format!("atrium-net: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    s.write_all(format!("route-domain name={vpn}\n").as_bytes()).map_err(|e| format!("atrium-net: {e}"))?;
    let mut fib = None;
    for line in BufReader::new(s).lines() {
        let line = line.map_err(|e| format!("atrium-net: {e}"))?;
        let mut w = line.split(' ');
        match w.next() {
            Some("ok") => return fib.ok_or_else(|| "atrium-net: no fib in the answer".into()),
            Some("error") => return Err(format!("atrium-net: {line}")),
            Some("domain") => {
                fib = w.find_map(|kv| kv.strip_prefix("fib=")).and_then(|v| v.parse::<u32>().ok()).filter(|f| (1..=8).contains(f));
            }
            _ => {}
        }
    }
    Err("atrium-net closed the connection".into())
}

/// The route domain for `person`'s `persona` (None or "personal": the
/// Personal persona): `Ok(None)` when its policy names no `vpn`, the FIB when
/// it does, and an error, which refuses the launch, when atrium-net gives
/// none. Never the host's own table instead.
pub fn for_persona(person: &str, persona: Option<&str>) -> Result<Option<u32>, String> {
    let Some(p) = persona.filter(|p| *p != portcullis_policy::PERSONAL) else { return Ok(None) };
    let vpn = portcullis_policy::Policy::load(&portcullis_policy::Policy::user_path(person)).ok()
        .and_then(|pol| pol.personas.get(p).and_then(|d| d.vpn.clone()));
    match vpn {
        None => Ok(None),
        Some(v) => fib_for(&v).map(Some).map_err(|e| format!("{p}'s VPN {v}: {e}")),
    }
}
