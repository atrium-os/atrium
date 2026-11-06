//! The jailed courier's VM gate (scripts/arca-vm-test.sh §5), run as an
//! app's uid in the guest:
//!
//!   [PROBE_HOLD=path] courierd-vm-probe SOCKET HOST PORT ITEM_HEX
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

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (sock, host, port, item) = (&a[1], &a[2], a[3].parse::<u16>().unwrap(), unhex(&a[4]));
    let before = courier_jails();
    let mut s = match arca_courierd::open(sock, host, port) {
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
