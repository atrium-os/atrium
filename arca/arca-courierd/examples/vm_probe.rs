//! The jailed courier's VM gate (scripts/arca-vm-test.sh §5), run as an
//! app's uid in the guest:
//!
//!   [PROBE_HOLD=path] courierd-vm-probe SOCKET HOST PORT ITEM_HEX
//!   courierd-vm-probe SOCKET HOST PORT web PARTITION ARCAD_SOCKET
//!
//! The second form runs as the web broker: a web courier for PARTITION,
//! then GET /me with tickets from arcad for three contexts (the reader's
//! own navigation, a cross-site subresource, a same-site navigation) and
//! one with no ticket. Prints `web<i>=<status>` per request.
//!
//! Asks arca-courierd for a courier to HOST:PORT, sends it GET /me with
//! ITEM as the bearer token and GET /redirect, reports what came back and
//! the courier's jail, closes the courier, and reports the jail gone.
//! Prints one `key=value` line per check.

use std::process::Command;
use std::time::{Duration, Instant};

use arca_courier::schema::{Header, Method, Place, Request, Response, Slot};
use arcad::proto::courier_op::SOURCE_ITEM;

fn unhex(s: &str) -> [u8; 16] {
    let mut o = [0u8; 16];
    for i in 0..16 {
        o[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    o
}

fn courier_jails() -> Vec<String> {
    let out = Command::new("jls").arg("name").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    out.lines().filter(|l| l.starts_with("system-arca-courier-")).map(String::from).collect()
}

fn ticket(arcad: &str, host: &str, port: u16, partition: &str, reason: u8, initiator: &str) -> Option<[u8; 16]> {
    use arca_store::codec::Writer;
    let mut w = Writer::new();
    arcad::proto::write_endpoint(&mut w, host, port);
    w.str(partition);
    w.u8(1);
    w.str("/me");
    w.u8(reason);
    w.str(initiator);
    w.u8(0);
    let mut s = std::os::unix::net::UnixStream::connect(arcad).ok()?;
    arcad::proto::write_request(&mut s, arcad::proto::broker_op::W_TICKET, &w.buf).ok()?;
    let (st, b) = arcad::proto::read_reply(&mut s).ok()?;
    (st == 0).then(|| b.as_ref().try_into().ok()).flatten()
}

fn web(sock: &str, host: &str, port: u16, partition: &str, arcad: &str) {
    let mut s = match arca_courierd::open(sock, host, port, Some(partition)) {
        Ok(s) => s,
        Err(code) => {
            println!("refused={code}");
            return;
        }
    };
    let ctx = [(1u8, ""), (2, "evil.example"), (1, "api.example"), (0, "")];
    for (i, (reason, initiator)) in ctx.iter().enumerate() {
        let t = if *reason == 0 { None } else { ticket(arcad, host, port, partition, *reason, initiator) };
        if *reason != 0 && t.is_none() {
            println!("web{i}=no-ticket");
            continue;
        }
        let r = Request {
            method: Method::Get,
            path: b"/me".to_vec(),
            query: vec![],
            headers: vec![],
            slots: vec![],
            body: vec![],
            ticket: t,
        };
        arca_courier::send::write_frame(&mut s, &r.encode()).unwrap();
        let f = arca_courier::send::read_frame(&mut s).unwrap().unwrap_or_default();
        match Response::decode(&f) {
            Some(Response::Http { status, .. }) => println!("web{i}={status}"),
            other => println!("web{i}={other:?}"),
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.get(4).map(String::as_str) == Some("web") {
        web(&a[1], &a[2], a[3].parse().unwrap(), &a[5], &a[6]);
        return;
    }
    let (sock, host, port, item) = (&a[1], &a[2], a[3].parse::<u16>().unwrap(), unhex(&a[4]));
    let before = courier_jails();
    let mut s = match arca_courierd::open(sock, host, port, None) {
        Ok(s) => s,
        Err(code) => {
            println!("refused={code}");
            return;
        }
    };
    let mut ask = |path: &str| -> Response {
        let r = Request {
            method: Method::Get,
            path: path.as_bytes().to_vec(),
            query: vec![],
            headers: vec![(Header::Accept, b"*/*".to_vec())],
            slots: vec![Slot { source: SOURCE_ITEM, reference: item, place: Place::Bearer }],
            body: vec![],
            ticket: None,
        };
        arca_courier::send::write_frame(&mut s, &r.encode()).unwrap();
        let f = arca_courier::send::read_frame(&mut s).unwrap().unwrap_or_default();
        Response::decode(&f).unwrap_or(Response::Refused(arca_courier::schema::Refusal::Internal))
    };
    match ask("/me") {
        Response::Http { status, body, .. } => {
            println!("status={status}");
            println!("body={}", String::from_utf8_lossy(&body));
        }
        Response::Refused(r) => println!("courier_refused={r:?}"),
    }
    match ask("/redirect") {
        Response::Http { status, headers, .. } => {
            let loc = headers.iter().find(|(k, _)| k == "location").map(|h| h.1.clone()).unwrap_or_default();
            println!("redirect={status} {loc}");
        }
        Response::Refused(r) => println!("redirect_refused={r:?}"),
    }
    let during: Vec<String> = courier_jails().into_iter().filter(|j| !before.contains(j)).collect();
    println!("jail={}", during.join(","));
    // PROBE_HOLD=path: keep the courier until that file exists, so the
    // gate can look at its jail.
    if let Ok(hold) = std::env::var("PROBE_HOLD") {
        let until = Instant::now() + Duration::from_secs(60);
        while Instant::now() < until && !std::path::Path::new(&hold).exists() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    drop(s);
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until && during.iter().any(|j| courier_jails().contains(j)) {
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("jail_gone={}", !during.iter().any(|j| courier_jails().contains(j)));
}
