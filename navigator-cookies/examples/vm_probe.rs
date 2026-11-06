//! The broker's cookie path in the VM (scripts/arca-vm-test.sh §7), run as
//! the web broker's uid, through the REAL arcad and courier launcher
//! sockets (`ArcadSocket`, `Launcher`) and jailed couriers:
//!
//!   cookies-vm-probe ARCAD_SOCKET COURIERD_SOCKET HOST PORT
//!
//! Prints one `key=value` line per step; the gate reads what the server
//! got from its log.
//!   1. an uncookied view: the plain way, no ticket asked for;
//!   2. cookied: the reader's own navigation to /c1?q=1;
//!   3. a cross-site subresource /c2 (from evil.example);
//!   4. the last ticket replayed through its courier (/c1?q=1 again);
//!   5. a fresh ticket for /c3?a=1 used for /c3?a=2, then for /c3?a=1.

use arca_courier::schema::{Header, Method, Request, Response};
use navigator_cookies::*;

/// arcad over its socket, keeping what it issued (to replay).
struct Keep { a: ArcadSocket, issued: Vec<(TicketAsk, [u8; 16])> }
impl Arca for Keep {
    fn ticket(&mut self, ask: &TicketAsk) -> Result<[u8; 16], String> {
        let t = self.a.ticket(ask)?;
        self.issued.push((ask.clone(), t));
        Ok(t)
    }
}

fn status(r: Result<Fetched, Refused>) -> String {
    match r { Ok(f) => f.status.to_string(), Err(e) => format!("{e:?}") }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (arcad, courierd, host, port) = (&a[1], &a[2], &a[3], a[4].parse::<u16>().unwrap());
    let base = format!("https://{host}:{port}");
    let mut b = Broker::new(Keep { a: ArcadSocket { path: arcad.into() }, issued: vec![] }, Launcher::new(courierd.clone()));
    b.show(1, &format!("{base}/")).unwrap();
    // 1. Uncookied.
    let r = b.fetch(1, &Fetch::navigation(&format!("{base}/c0"), None, true));
    println!("uncookied={} tickets={}", status(r), b.tickets);
    // 2. Cookied: the reader's own navigation.
    b.set_cookied(1, Why::Reader).unwrap();
    println!("nav={}", status(b.fetch(1, &Fetch::navigation(&format!("{base}/c1?q=1"), None, true))));
    // 3. A cross-site subresource.
    println!("cross={}", status(b.fetch(1, &Fetch::subresource(&format!("{base}/c2"), "evil.example"))));
    // 4. The navigation's ticket, replayed through the same courier.
    let (ask, used) = b.arca.issued.iter().find(|(x, _)| x.path == "/c1").cloned().unwrap();
    let req = |path: &str, query: &str, t| Request { method: Method::Get, path: path.as_bytes().to_vec(), query: query.as_bytes().to_vec(),
        headers: vec![(Header::Accept, b"*/*".to_vec())], slots: vec![], body: vec![], ticket: Some(t) };
    let mut send = |r: Request| match b.couriers.send(host, port, &ask.partition, &r) {
        Ok(Response::Http { status, .. }) => status.to_string(),
        Ok(other) => format!("{other:?}"),
        Err(e) => e,
    };
    println!("replay={}", send(req("/c1", "q=1", used)));
    // 5. A fresh ticket for /c3?a=1: not for /c3?a=2; still for itself.
    let mut ask3 = ask.clone();
    ask3.path = "/c3".into();
    ask3.query = "a=1".into();
    let t = ArcadSocket { path: arcad.into() }.ticket(&ask3).unwrap();
    println!("other_query={}", send(req("/c3", "a=2", t)));
    println!("own_query={}", send(req("/c3", "a=1", t)));
}
