//! atrium-net's control lines (`status`, `wifi-list`, aura-mlme's `bss`)
//! turned into the wire types, and back. Pure, so tested on the host.

use aura_ipc::Line;

use crate::{bars, Iface, Link, Nearby, Online, Profile, Routes, Status};

fn list(l: &Line, k: &str) -> Vec<String> {
    l.get(k).map(|v| v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()
}

fn online(w: Option<&str>) -> Online {
    match w {
        Some("yes") => Online::Yes,
        Some("no") => Online::No,
        Some("captive") => Online::Captive,
        _ => Online::Unknown,
    }
}

/// One `iface` line of atrium-net's `status`.
pub fn iface(l: &Line) -> Option<Iface> {
    if l.verb != "iface" {
        return None;
    }
    let bound = l.get("state") == Some("bound");
    Some(Iface {
        name: l.get("name")?.to_string(),
        mac: l.get("mac").unwrap_or("").to_string(),
        wifi: l.get("wifi") == Some("1"),
        link: l.get("link").unwrap_or("down").to_string(),
        addr: l.get("addr").filter(|_| bound).map(String::from),
        router: l.get("router").filter(|_| bound).map(String::from),
        dns: list(l, "dns"),
        addrs6: list(l, "inet6"),
        dns6: list(l, "dns6"),
        online: online(l.get("internet")),
        metered: l.get("metered") == Some("1"),
        egress: l.get("egress") == Some("1"),
    })
}

/// The bar's view from a whole `status` reply; `rssi`: aura-mlme's last
/// signal reading while connected.
pub fn status(lines: &[Line], rssi: Option<i32>) -> Status {
    let ifs: Vec<Iface> = lines.iter().filter_map(iface).collect();
    let Some((i, l)) = ifs.iter().zip(lines.iter().filter(|l| l.verb == "iface")).find(|(i, _)| i.egress) else {
        return Status::offline();
    };
    let wifi = lines.iter().find(|l| l.verb == "wifi");
    Status {
        link: if i.wifi { Link::Wifi } else { Link::Wired },
        online: i.online,
        metered: i.metered,
        bars: if i.wifi { rssi.map(bars) } else { None },
        ssid: if i.wifi { wifi.and_then(|w| w.hex("ssid")) } else { None },
        portal: l.hex("portal").and_then(|p| String::from_utf8(p).ok()),
    }
}

pub fn routes(lines: &[Line]) -> Routes {
    let e = lines.iter().filter_map(iface).find(|i| i.egress);
    Routes { default4: e.and_then(|i| Some((i.name.clone(), i.router.clone()?))) }
}

/// A `net` line of `wifi-list`.
pub fn profile(l: &Line) -> Option<Profile> {
    if l.verb != "net" {
        return None;
    }
    Some(Profile {
        ssid: l.hex("ssid").filter(|s| !s.is_empty() && s.len() <= 32)?,
        open: l.get("open") == Some("1"),
        wpa3_only: l.get("wpa3_only") == Some("1"),
        mac: l.get("mac").unwrap_or("network").to_string(),
        auto: l.get("auto") != Some("0"),
        prio: l.num("prio").unwrap_or(0),
        metered: l.get("metered") == Some("1"),
    })
}

/// `wifi-remember` for a profile's settings (no secret: the credential
/// already in aura-keys stays).
pub fn remember(p: &Profile) -> Line {
    Line::new("wifi-remember")
        .with_hex("ssid", &p.ssid)
        .with("open", p.open as u8)
        .with("mac", &p.mac)
        .with("auto", p.auto as u8)
        .with("prio", p.prio.clamp(-1000, 1000))
        .with("metered", p.metered as u8)
}

/// What the user calls aura-mlme's security names.
pub fn security(s: &str) -> &'static str {
    match s {
        "open" => "open",
        "psk" | "psk-sha256" => "wpa2",
        "sae" => "wpa3",
        "sae-transition" => "wpa2/wpa3",
        _ => "unsupported",
    }
}

/// A scan's `bss` lines: one entry per name (the strongest), hidden
/// networks left out, strongest first. BSSIDs and channels are dropped.
pub fn nearby(bss: &[Line], known: &[Vec<u8>]) -> Vec<Nearby> {
    let mut best: Vec<(Vec<u8>, i32, &'static str)> = Vec::new();
    for l in bss.iter().filter(|l| l.verb == "bss") {
        let (Some(ssid), Some(rssi)) = (l.hex("ssid"), l.num::<i32>("rssi")) else { continue };
        if ssid.is_empty() || ssid.len() > 32 || ssid.iter().all(|&c| c == 0) {
            continue;
        }
        let sec = security(l.get("security").unwrap_or(""));
        match best.iter_mut().find(|b| b.0 == ssid) {
            Some(b) if rssi > b.1 => *b = (ssid, rssi, sec),
            Some(_) => {}
            None => best.push((ssid, rssi, sec)),
        }
    }
    best.sort_by(|a, b| b.1.cmp(&a.1));
    best.into_iter()
        .take(64)
        .map(|(ssid, rssi, sec)| Nearby { known: known.contains(&ssid), ssid, security: sec.to_string(), bars: bars(rssi) })
        .collect()
}
