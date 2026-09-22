//! A networked jail's own stack: a point-to-point epair (network.md §0).
//!
//! Every jail granted network gets `vnet=new` plus one end of an epair, on a
//! /30 of its own from `100.64.0.0/16`: host end `.1` (in interface group
//! `atrium`), app end `.2` with a per-app derived MAC, default route via `.1`.
//! Point-to-point rather than a bridge, so jails share no layer 2 and all
//! traffic between them is routed through the host, where pf sees it.
//!
//! ★★ pf's base rules are what keep a jail off the host and off other jails —
//! measured: without them a jail reached the host's sshd through its gateway.
//! So [`isolation_loaded`] is checked before any networked jail is created, the
//! same fail-closed shape as the devfs rulesets (portcullis.md §9.1b).
//!
//! Shelled (`ifconfig`, `route`, `pfctl`) in the audited style this crate
//! already uses for `ifconfig` aliases and `rctl`: the interface ioctls are
//! stable but wide, and the jail-scoped forms (`-j`) do the vnet switch for us.

use std::io;
use std::process::Command;

/// /30 slots in 100.64.0.0/16.
pub const SLOTS: u32 = 16384;

/// Host-end and app-end addresses of `slot`.
pub fn slot_addrs(slot: u32) -> (String, String) {
    let base = slot * 4;
    let (o3, o4) = (base / 256, base % 256);
    (format!("100.64.{o3}.{}", o4 + 1), format!("100.64.{o3}.{}", o4 + 2))
}

/// Whether a MAC is a plain locally-administered unicast address — the only
/// kind a derived identity produces (portcullis-identity).
pub fn valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 || !parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())) {
        return false;
    }
    let b0 = u8::from_str_radix(parts[0], 16).unwrap_or(1);
    b0 & 0x01 == 0 && b0 & 0x02 == 0x02
}

fn sh(cmd: &str, args: &[&str]) -> io::Result<String> {
    let out = Command::new(cmd).args(args).output()?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(io::Error::other(format!("{cmd} {}: {}", args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim())))
}

/// ★★ The isolation a networked jail depends on is in force: pf enabled, both
/// block rules on group `atrium` loaded, and forwarding on (without it the jail
/// has no route out — not a leak, but a jail that silently cannot work).
pub fn isolation_loaded() -> Result<(), String> {
    let info = sh("pfctl", &["-s", "info"]).map_err(|e| format!("pf: {e}"))?;
    if !info.lines().any(|l| l.trim_start().starts_with("Status: Enabled")) {
        return Err("pf is not enabled".into());
    }
    let rules = sh("pfctl", &["-s", "rules"]).map_err(|e| format!("pf rules: {e}"))?;
    let has = |needle: &str| rules.lines().any(|l| l.contains("block drop in quick on atrium") && l.contains(needle));
    if !has("to (self)") || !has("to 100.64.0.0/16") {
        return Err("pf is missing the atrium isolation rules (no app->host / app->app blocks); \
                    load etc/atrium.pf".into());
    }
    let fwd = sh("sysctl", &["-n", "net.inet.ip.forwarding"]).map_err(|e| e.to_string())?;
    if fwd.trim() != "1" {
        return Err("net.inet.ip.forwarding is 0; a networked jail would have no route out".into());
    }
    Ok(())
}

/// Create the epair and configure the host end. Returns (a, b).
pub fn create_host_end(slot: u32) -> io::Result<(String, String)> {
    let a = sh("ifconfig", &["epair", "create"])?.trim().to_string();
    let b = format!("{}b", a.strip_suffix('a').unwrap_or(&a));
    let (host, _) = slot_addrs(slot);
    if let Err(e) = sh("ifconfig", &[&a, "inet", &format!("{host}/30"), "up", "group", "atrium"]) {
        let _ = destroy(&a);
        return Err(e);
    }
    Ok((a, b))
}

/// Move `b` into the jail's vnet and configure the app end: the derived MAC,
/// the /30 address, its own loopback, the default route. Done before anything
/// runs in the jail.
pub fn configure_app_end(jail: &str, b: &str, mac: &str, slot: u32) -> io::Result<()> {
    let (host, app) = slot_addrs(slot);
    sh("ifconfig", &[b, "vnet", jail])?;
    sh("ifconfig", &["-j", jail, b, "ether", mac])?;
    sh("ifconfig", &["-j", jail, b, "inet", &format!("{app}/30"), "up"])?;
    sh("ifconfig", &["-j", jail, "lo0", "inet", "127.0.0.1/8", "up"])?;
    sh("route", &["-j", jail, "add", "default", &host])?;
    Ok(())
}

/// Destroy the pair (destroying either end destroys both).
pub fn destroy(a: &str) -> io::Result<()> {
    sh("ifconfig", &[a, "destroy"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_disjoint_slash30s() {
        assert_eq!(slot_addrs(0), ("100.64.0.1".into(), "100.64.0.2".into()));
        assert_eq!(slot_addrs(1), ("100.64.0.5".into(), "100.64.0.6".into()));
        assert_eq!(slot_addrs(64), ("100.64.1.1".into(), "100.64.1.2".into()));
        assert_eq!(slot_addrs(SLOTS - 1), ("100.64.255.253".into(), "100.64.255.254".into()));
    }

    #[test]
    fn only_locally_administered_unicast_macs_are_accepted() {
        assert!(valid_mac("02:a7:10:00:00:01"));
        assert!(!valid_mac("00:1b:21:12:34:56"), "a vendor (universally administered) MAC");
        // ★ QEMU's 52:54:00 prefix is itself LOCALLY administered, so this is a
        // SHAPE check only: it cannot tell a derived MAC from a VM's real one.
        // What keeps the real MAC out is that callers derive it from the
        // machine secret (portcullis-identity), never read it from a NIC.
        assert!(valid_mac("52:54:00:12:34:56"));
        assert!(!valid_mac("03:00:00:00:00:01"), "multicast");
        assert!(!valid_mac("02:A7:10:00:00:01"), "uppercase");
        assert!(!valid_mac("02:a7:10:00:00"), "short");
    }
}
