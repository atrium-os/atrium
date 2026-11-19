//! Printers on the LAN (printing.md §7.1).
//!
//! Only while someone is looking: the first `NEARBY` starts a
//! `prelum-browse` and hands it atrium-net's mDNS sockets; it is stopped
//! once nobody has asked for [`IDLE`]. What multicast says is only where to
//! ask: each new service is confirmed by asking the printer itself over IPP
//! (a `prelum-ipp`, learning its key the way adding does), and only
//! printers that answered are listed, as they described themselves. At
//! most [`CONFIRMING`] confirmations run at once and [`MAX_CONFIRM`] per
//! browsing session, so a flood of announcements costs a bounded number of
//! workers.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use prelum_dnssd::Service;
use prelum_ipp_codec::printer::PrinterInfo;
use prelum_proto::browse;
use prelum_proto::frame;
use prelum_proto::print::{self, status};

use crate::daemon::{fingerprint, Daemon, Fail};
use crate::launch::{send_fds, Stopper};
use crate::store::{self, Saved};

/// Nobody asked for this long: the browser stops.
pub const IDLE: Duration = Duration::from_secs(60);
/// Confirmations at once, and per browsing session.
pub const CONFIRMING: usize = 4;
pub const MAX_CONFIRM: usize = 64;
/// A printer that hasn't answered who it is in this long isn't listed.
pub const CONFIRM_LIMIT: Duration = Duration::from_secs(8);
/// The longest a `NEARBY` listens.
pub const MAX_WAIT: u8 = 10;

/// The two sockets prelum-browse needs (`prelum_proto::browse`).
pub struct MdnsSockets {
    pub recv: OwnedFd,
    pub send: OwnedFd,
    /// One socket connected to a responder (development): its answers live
    /// 10 s, so the worker asks more often.
    pub legacy: bool,
}

/// What a confirmed printer said.
#[derive(Clone)]
struct Confirmed {
    uri: String,
    tls: bool,
    announced: String,
    info: PrinterInfo,
    key: Option<[u8; 32]>,
}

#[derive(Clone)]
enum Found {
    Pending,
    Ok(Box<Confirmed>),
    Failed,
}

struct Worker {
    sock: UnixStream,
    stop: Stopper,
}

#[derive(Default)]
pub struct Near {
    worker: Option<Worker>,
    /// Each browsing session's number; a thread of an older one stops.
    session: u64,
    last_asked: Option<Instant>,
    /// What the browser last listed, by URI.
    heard: HashMap<String, Service>,
    found: HashMap<String, Found>,
    queue: VecDeque<(String, Service)>,
    confirming: usize,
    started: usize,
    error: Option<String>,
    legacy: bool,
}

/// Where to ask a service: `ipps://a.b.c.d:port/rp` (IPv4 only: a worker
/// can't be granted an IPv6 printer yet). `None` when it gave no usable
/// address or path. A loopback address is the LAN pointing us at
/// ourselves, refused unless `loopback` (legacy unicast to this host's own
/// responder, in development).
pub fn uri_of(s: &Service, loopback: bool) -> Option<String> {
    let tls = s.service_type.to_ascii_lowercase().starts_with("_ipps.");
    let ip = s.addrs.iter().find_map(|a| match a {
        IpAddr::V4(v) if !v.is_unspecified() && !v.is_broadcast() && !v.is_multicast() && (loopback || !v.is_loopback()) => Some(Ipv4Addr::from(v.octets())),
        _ => None,
    })?;
    if s.port == 0 {
        return None;
    }
    let rp = s.txt.rp.as_deref().unwrap_or("ipp/print").trim_start_matches('/');
    if rp.len() > 128 || !rp.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b)) || rp.contains("..") {
        return None;
    }
    Some(format!("{}://{}:{}/{}", if tls { "ipps" } else { "ipp" }, ip, s.port, rp))
}

fn lock(d: &Daemon) -> std::sync::MutexGuard<'_, Near> {
    d.near.lock().unwrap_or_else(|e| e.into_inner())
}

impl Daemon {
    /// `NEARBY`: listen up to `secs` seconds, then list what answered.
    pub fn nearby(self: &Arc<Self>, uid: u32, persona: Option<&str>, secs: u8) -> Result<Vec<print::Nearby>, Fail> {
        if let Some(p) = persona {
            if self.route(uid, p)?.is_some() {
                return Err((status::NOT_ALLOWED, "Nearby discovery is unavailable on a VPN persona".into()));
            }
        }
        let wait = Duration::from_secs(secs.min(MAX_WAIT) as u64);
        self.browse()?;
        std::thread::sleep(wait);
        let list: Vec<Confirmed> = {
            let n = lock(self);
            if n.worker.is_none() {
                if let Some(e) = &n.error {
                    return Err((status::FAILED, e.clone()));
                }
            }
            let mut v: Vec<Confirmed> = n
                .found
                .iter()
                .filter(|(uri, _)| n.heard.contains_key(*uri))
                .filter_map(|(_, f)| match f {
                    Found::Ok(c) => Some((**c).clone()),
                    _ => None,
                })
                .collect();
            // One printer offering both IPPS and IPP (the same printer-uuid
            // on the same address) is listed once, by IPPS (§7.2).
            let host = |u: &str| u.split("://").nth(1).and_then(|r| r.split(':').next()).unwrap_or("").to_string();
            let secure: Vec<(String, String)> =
                v.iter().filter(|c| c.tls).filter_map(|c| c.info.uuid.clone().map(|u| (u, host(&c.uri)))).collect();
            v.retain(|c| c.tls || !c.info.uuid.as_ref().is_some_and(|u| secure.contains(&(u.clone(), host(&c.uri)))));
            v.sort_by(|a, b| a.announced.cmp(&b.announced).then(a.uri.cmp(&b.uri)));
            v
        };
        Ok(list
            .into_iter()
            .map(|c| {
                let key = c.key.map(|k| store::hex(&k));
                let i = &c.info;
                print::Nearby {
                    name: [&i.info, &i.make_and_model, &c.announced].into_iter().find(|n| !n.is_empty()).cloned().unwrap_or_default(),
                    make_and_model: i.make_and_model.clone(),
                    location: i.location.clone(),
                    announced: c.announced.clone(),
                    tls: c.tls,
                    fingerprint: c.key.as_ref().map(fingerprint),
                    uuid: i.uuid.clone(),
                    color: i.color_modes.iter().any(|m| m == "color"),
                    duplex: i.sides_supported.iter().any(|s| s != "one-sided"),
                    saved: self.saved_as(uid, persona, i.uuid.as_deref(), key.as_deref()),
                    uri: c.uri,
                }
            })
            .collect())
    }

    /// Start the browser, or ask it to ask again.
    fn browse(self: &Arc<Self>) -> Result<(), Fail> {
        let mut n = lock(self);
        n.last_asked = Some(Instant::now());
        if let Some(w) = n.worker.as_mut() {
            if frame::write(&mut w.sock, &browse::Req::Query).is_ok() {
                return Ok(());
            }
            w.stop.stop();
            n.worker = None;
        }
        let Some(src) = self.mdns_source() else {
            return Err((status::FAILED, "printers can't be looked for here (no mDNS)".into()));
        };
        let socks = src().map_err(|e| (status::OFFLINE, format!("no local network to look on: {e}")))?;
        let args: Vec<String> = if socks.legacy { vec!["--legacy".into()] } else { vec![] };
        let p = self.launcher().browse(&args).map_err(|e| (status::FAILED, format!("can't start prelum-browse: {e}")))?;
        send_fds(&p.sock, &[&socks.recv, &socks.send]).map_err(|e| {
            p.stop.stop();
            (status::FAILED, format!("prelum-browse: {e}"))
        })?;
        let reader = p.sock.try_clone().map_err(|e| (status::FAILED, e.to_string()))?;
        n.session += 1;
        n.heard.clear();
        n.found.clear();
        n.queue.clear();
        n.started = 0;
        n.error = None;
        n.legacy = socks.legacy;
        n.worker = Some(Worker { sock: p.sock, stop: p.stop });
        let session = n.session;
        drop(n);
        drop(socks);
        let d = self.clone();
        std::thread::spawn(move || d.read_browse(session, reader));
        let d = self.clone();
        std::thread::spawn(move || d.idle_stop(session));
        Ok(())
    }

    fn read_browse(self: Arc<Self>, session: u64, mut r: UnixStream) {
        let why = loop {
            match frame::read::<browse::Ev>(&mut r) {
                Ok(Some(browse::Ev::Services(list))) => {
                    let mut n = lock(&self);
                    if n.session != session {
                        return;
                    }
                    n.heard = list.into_iter().take(prelum_dnssd::MAX_SERVICES).filter_map(|s| uri_of(&s, n.legacy).map(|u| (u, s))).collect();
                    let new: Vec<(String, Service)> =
                        n.heard.iter().filter(|(u, _)| !n.found.contains_key(*u)).map(|(u, s)| (u.clone(), s.clone())).collect();
                    for (u, s) in new {
                        if n.started + n.queue.len() >= MAX_CONFIRM {
                            break;
                        }
                        n.found.insert(u.clone(), Found::Pending);
                        n.queue.push_back((u, s));
                    }
                    self.confirm_more(&mut n, session);
                }
                Ok(Some(browse::Ev::Failed(m))) => break m,
                Ok(None) => break "prelum-browse ended".into(),
                Err(e) => break format!("prelum-browse: {e}"),
            }
        };
        let mut n = lock(&self);
        if n.session == session {
            if let Some(w) = n.worker.take() {
                w.stop.stop();
            }
            n.error = Some(crate::daemon::short(&why));
        }
    }

    fn confirm_more(self: &Arc<Self>, n: &mut Near, session: u64) {
        while n.confirming < CONFIRMING {
            let Some((uri, svc)) = n.queue.pop_front() else { return };
            n.confirming += 1;
            n.started += 1;
            let d = self.clone();
            std::thread::spawn(move || {
                let r = d.confirm(&uri, &svc);
                let mut n = lock(&d);
                if n.session != session {
                    return;
                }
                n.confirming -= 1;
                n.found.insert(uri.to_string(), r.map(Box::new).map(Found::Ok).unwrap_or(Found::Failed));
                d.confirm_more(&mut n, session);
            });
        }
    }

    /// Ask the printer itself who it is (learning its key, as adding does).
    fn confirm(&self, uri: &str, svc: &Service) -> Option<Confirmed> {
        let tls = uri.starts_with("ipps://");
        let hostport = uri.split("://").nth(1)?.split('/').next()?;
        let addr: SocketAddr = hostport.parse().ok()?;
        let s = Saved {
            id: 0,
            name: String::new(),
            uri: uri.to_string(),
            addr: addr.to_string(),
            tls,
            key: None,
            uuid: None,
            network: self.network_now(),
            raster_only: false,
            titles_off: false,
            jobs_sent: 0,
            persona: String::new(),
            shared: false,
            info: PrinterInfo::default(),
        };
        let (info, key) = self.learn_within(&s, Some(CONFIRM_LIMIT), None).ok()?;
        Some(Confirmed { uri: uri.to_string(), tls, announced: svc.instance.clone(), info, key })
    }

    fn idle_stop(self: Arc<Self>, session: u64) {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let mut n = lock(&self);
            if n.session != session || n.worker.is_none() {
                return;
            }
            if n.last_asked.is_some_and(|t| t.elapsed() >= IDLE) {
                if let Some(w) = n.worker.take() {
                    let _ = w.sock.shutdown(std::net::Shutdown::Both);
                    w.stop.stop();
                }
                n.session += 1;
                n.heard.clear();
                n.found.clear();
                n.queue.clear();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prelum_dnssd::Txt;

    fn svc(t: &str, addrs: Vec<IpAddr>, port: u16, rp: Option<&str>) -> Service {
        Service {
            instance: "P".into(),
            service_type: t.into(),
            host: "p.local".into(),
            port,
            addrs,
            txt: Txt { rp: rp.map(String::from), ..Txt::default() },
        }
    }

    #[test]
    fn where_to_ask_a_service() {
        let a = |o: u8| IpAddr::V4(Ipv4Addr::new(192, 168, 1, o));
        assert_eq!(uri_of(&svc("_ipps._tcp.local", vec![a(20)], 631, Some("ipp/print")), false).as_deref(), Some("ipps://192.168.1.20:631/ipp/print"));
        assert_eq!(uri_of(&svc("_ipp._tcp.local", vec![a(20)], 8631, None), false).as_deref(), Some("ipp://192.168.1.20:8631/ipp/print"));
        assert_eq!(uri_of(&svc("_IPPS._tcp.local", vec!["fe80::1".parse().unwrap(), a(7)], 631, Some("/ipp/print")), false).as_deref(), Some("ipps://192.168.1.7:631/ipp/print"));
        assert_eq!(uri_of(&svc("_ipps._tcp.local", vec!["fe80::1".parse().unwrap()], 631, None), false), None, "IPv6 only");
        assert_eq!(uri_of(&svc("_ipps._tcp.local", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)], 631, None), false), None);
        assert!(uri_of(&svc("_ipps._tcp.local", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)], 631, None), true).is_some(), "development");
        assert_eq!(uri_of(&svc("_ipps._tcp.local", vec![a(20)], 0, None), false), None);
        for bad in ["../etc", "ipp/print?x=1", "a b", "ipp/print#frag", "x@y"] {
            assert_eq!(uri_of(&svc("_ipps._tcp.local", vec![a(20)], 631, Some(bad)), false), None, "{bad}");
        }
        assert_eq!(uri_of(&svc("_ipps._tcp.local", vec![a(20)], 631, Some(&"a".repeat(200))), false), None);
    }
}
