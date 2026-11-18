//! The captive portal sign-in window's fetcher (atrium-net.md §8.1): what
//! portcullisd asks atrium-netconf on its root-only `network-portal` socket.
//! The network's DNS servers come from here, never from the caller.

use std::path::PathBuf;

use atrium_netconf::{client::Client, Grant, PortalInfo};

/// `/atrium/sockets`, or `PORTCULLISD_NETCONF_DIR` for tests.
fn dir() -> PathBuf {
    std::env::var_os("PORTCULLISD_NETCONF_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/atrium/sockets"))
}

fn client() -> Result<Client, String> {
    let p = Client::path(&dir(), Grant::Portal);
    Client::connect(&p).map_err(|e| format!("atrium-net is not answering on {}: {e}", p.display()))
}

/// The network the window is for: its interface, DNS servers and where to start.
pub fn info() -> Result<PortalInfo, String> {
    client()?.portal_info().map_err(|f| f.1)
}

/// Start or stop atrium-net's quick re-probing while the window is open.
/// Best effort: the window works without it, sign-in just shows later.
pub fn watch(on: bool) {
    if let Err(e) = client().and_then(|mut c| c.portal_watch(on).map_err(|f| f.1)) {
        eprintln!("portcullisd: portal watch {}: {e}", if on { "on" } else { "off" });
    }
}

/// The host's own addresses (every interface's, link-local zones dropped),
/// comma-separated, for the fetcher's `ATRIUM_NUNTIUS_OWN`: its jail sees
/// only its own interfaces, and the window must reach none of the machine's.
#[allow(unsafe_code)]
pub fn host_addresses() -> String {
    let mut out: Vec<String> = Vec::new();
    let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `ifa` with a list freed below; each entry's
    // address is read according to its family.
    unsafe {
        if libc::getifaddrs(&mut ifa) != 0 {
            return String::new();
        }
        let mut p = ifa;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() {
                match (*a).sa_family as i32 {
                    libc::AF_INET => {
                        let s = &*(a as *const libc::sockaddr_in);
                        out.push(std::net::Ipv4Addr::from(u32::from_be(s.sin_addr.s_addr)).to_string());
                    }
                    libc::AF_INET6 => {
                        let s = &*(a as *const libc::sockaddr_in6);
                        out.push(std::net::Ipv6Addr::from(s.sin6_addr.s6_addr).to_string());
                    }
                    _ => {}
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifa);
    }
    out.dedup();
    out.join(",")
}
