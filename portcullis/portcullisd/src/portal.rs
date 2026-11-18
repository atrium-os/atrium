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
