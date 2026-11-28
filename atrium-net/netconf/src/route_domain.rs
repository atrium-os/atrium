//! Root-only client for atrium-net route domains.

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

/// The only affirmative reply from the root-only source-tunnel broker.
pub fn link_may_leave(vpn: &str, host: &str) -> bool {
    use std::io::Read;
    let query = || -> std::io::Result<bool> {
        if vpn.is_empty() || vpn.len() > 32 || !vpn.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || host.is_empty() || host.len() > 253 || !host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'.')) {
            return Ok(false);
        }
        let path = std::env::var_os("ATRIUM_NET_SOCK").unwrap_or_else(|| SOCK.into());
        let mut socket = UnixStream::connect(path)?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        writeln!(socket, "tunnel-link name={vpn} host={host}")?;
        let mut answer = [0u8; 9];
        socket.read_exact(&mut answer)?;
        Ok(&answer == b"clear\nok\n")
    };
    query().unwrap_or(false)
}
