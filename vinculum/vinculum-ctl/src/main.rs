//! vinculum-ctl: the `bluetooth-control` CLI and pairing agent (spec §13).
//!
//! ```text
//! vinculum-ctl [-s SOCKET] status
//! vinculum-ctl [-s SOCKET] devices
//! vinculum-ctl [-s SOCKET] scan [SECS]
//! vinculum-ctl [-s SOCKET] pair NAME|ADDR [--yes] [--timeout SECS]
//! vinculum-ctl [-s SOCKET] connect|disconnect|remove ADDR
//! vinculum-ctl [-s SOCKET] agent [--yes]
//! ```
//!
//! `pair` scans for a device by name (or takes its address), pairs it and
//! acts as the pairing agent meanwhile: it shows the number to compare or
//! the passkey to type and asks on the terminal, or answers yes with
//! `--yes` (scripts and tests; on a real machine the point of the prompt
//! is that a person checks it). `agent` stays connected and answers prompts
//! for pairings started elsewhere.

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use vinculum_control::{encode, Addr, Event, Prompt, Reader, Request};

const DEFAULT_SOCKET: &str = "/var/run/vinculum/control.sock";

struct Conn {
    sock: UnixStream,
    reader: Reader,
}

impl Conn {
    fn open(path: &str, agent: bool) -> Conn {
        let sock = UnixStream::connect(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        let mut c = Conn { sock, reader: Reader::default() };
        c.call(Request::Hello { agent });
        c
    }

    fn send(&mut self, r: &Request) {
        self.sock.write_all(&encode(r)).unwrap_or_else(|e| die(&format!("send: {e}")));
    }

    /// The next event, or None at the deadline.
    fn next(&mut self, deadline: Option<Instant>) -> Option<Event> {
        loop {
            match self.reader.next::<Event>() {
                Ok(Some(e)) => return Some(e),
                Ok(None) => {}
                Err(_) => die("malformed message from vinculumd"),
            }
            let t = match deadline {
                Some(d) => Some(d.checked_duration_since(Instant::now()).filter(|t| !t.is_zero())?),
                None => None,
            };
            self.sock.set_read_timeout(t).expect("timeout");
            let mut buf = [0u8; 4096];
            match self.sock.read(&mut buf) {
                Ok(0) => die("vinculumd closed the connection"),
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return None,
                Err(e) => die(&format!("read: {e}")),
            }
        }
    }

    /// Sends a request and returns its reply, printing unsolicited events
    /// that arrive first.
    fn call(&mut self, r: Request) -> Event {
        self.send(&r);
        loop {
            let e = self.next(Some(Instant::now() + Duration::from_secs(10))).unwrap_or_else(|| die("no reply"));
            match e {
                Event::Ok | Event::Error { .. } | Event::Status { .. } | Event::Devices { .. } => return e,
                other => print_event(&other),
            }
        }
    }

    fn ok(&mut self, r: Request) {
        if let Event::Error { why } = self.call(r) {
            die(&why);
        }
    }
}

fn die(why: &str) -> ! {
    eprintln!("vinculum-ctl: {why}");
    std::process::exit(1)
}

fn print_event(e: &Event) {
    match e {
        Event::Found { address, name, rssi } => println!("found {address} {} (rssi {rssi})", name.as_deref().unwrap_or("-")),
        Event::Connected { address, encrypted } => println!("connected {address}{}", if *encrypted { " (encrypted)" } else { "" }),
        Event::Disconnected { address, reason } => println!("disconnected {address} ({reason})"),
        Event::Paired { address, authenticated, keyboard_allowed } => println!(
            "paired {address} ({}; keyboard {})",
            if *authenticated { "authenticated" } else { "unauthenticated" },
            if *keyboard_allowed { "allowed" } else { "not allowed" }
        ),
        Event::PairingFailed { address, why } => println!("pairing {address} failed: {why}"),
        Event::Prompt { address, prompt } => println!("prompt {address}: {prompt:?}"),
        Event::Error { why } => println!("error: {why}"),
        other => println!("{other:?}"),
    }
}

fn ask(q: &str) -> String {
    print!("{q} ");
    let _ = std::io::stdout().flush();
    let mut l = String::new();
    std::io::stdin().lock().read_line(&mut l).unwrap_or(0);
    l.trim().to_string()
}

/// Answers one prompt as the agent.
fn answer(c: &mut Conn, address: Addr, p: Prompt, yes: bool) {
    match p {
        Prompt::Confirm { number } => {
            let ok = match number {
                Some(n) => {
                    println!("{address}: does the device show {n:06}?");
                    yes || ask("[y/N]").eq_ignore_ascii_case("y")
                }
                None => {
                    println!("{address}: pair (no number to compare: Just Works)?");
                    yes || ask("[y/N]").eq_ignore_ascii_case("y")
                }
            };
            c.send(&Request::Confirm { address, yes: ok });
        }
        Prompt::Display { passkey } => println!("{address}: type {passkey:06} on the device"),
        Prompt::EnterPasskey => {
            let p = if yes { None } else { ask("passkey shown on the device:").parse().ok() };
            c.send(&Request::Passkey { address, passkey: p });
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket = std::env::var("VINCULUM_CONTROL").unwrap_or_else(|_| DEFAULT_SOCKET.into());
    if args.first().map(String::as_str) == Some("-s") && args.len() > 1 {
        socket = args[1].clone();
        args.drain(..2);
    }
    let flag = |a: &mut Vec<String>, f: &str| a.iter().position(|x| x == f).map(|i| a.remove(i)).is_some();
    let yes = flag(&mut args, "--yes");
    let timeout = match args.iter().position(|x| x == "--timeout") {
        Some(i) if i + 1 < args.len() => {
            let v = args[i + 1].parse().unwrap_or_else(|_| die("--timeout SECS"));
            args.drain(i..i + 2);
            v
        }
        _ => 30u64,
    };
    let addr = |a: Option<&String>| -> Addr { a.unwrap_or_else(|| die("needs an address")).parse().unwrap_or_else(|e| die(e)) };
    match args.first().map(String::as_str) {
        Some("status") => {
            let mut c = Conn::open(&socket, false);
            println!("{:?}", c.call(Request::Status));
        }
        Some("devices") => {
            let mut c = Conn::open(&socket, false);
            if let Event::Devices { devices } = c.call(Request::Devices) {
                for d in devices {
                    println!(
                        "{} {:?} {}{}{}",
                        d.address,
                        d.name,
                        if d.authenticated { "authenticated" } else { "unauthenticated" },
                        if d.keyboard_allowed { ", keyboard allowed" } else { "" },
                        if d.connected { ", connected" } else { "" }
                    );
                }
            }
        }
        Some("scan") => {
            let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let mut c = Conn::open(&socket, false);
            c.ok(Request::Scan { on: true });
            let end = Instant::now() + Duration::from_secs(secs);
            while let Some(e) = c.next(Some(end)) {
                print_event(&e);
            }
            c.ok(Request::Scan { on: false });
        }
        Some("connect") => {
            // Waits until the link is encrypted with the bond.
            let a = addr(args.get(1));
            let mut c = Conn::open(&socket, false);
            c.ok(Request::Connect { address: a });
            let end = Instant::now() + Duration::from_secs(timeout);
            loop {
                match c.next(Some(end)) {
                    Some(e @ Event::Connected { address, encrypted: true }) if address == a => {
                        print_event(&e);
                        return;
                    }
                    Some(e @ (Event::Error { .. } | Event::Disconnected { .. })) => {
                        print_event(&e);
                        std::process::exit(1);
                    }
                    Some(e) => print_event(&e),
                    None => die("connect timed out"),
                }
            }
        }
        Some(cmd @ ("disconnect" | "remove")) => {
            let a = addr(args.get(1));
            let mut c = Conn::open(&socket, false);
            c.ok(if cmd == "disconnect" { Request::Disconnect { address: a } } else { Request::Remove { address: a } });
        }
        Some("pair") => {
            let target = args.get(1).unwrap_or_else(|| die("pair NAME|ADDR")).clone();
            let mut c = Conn::open(&socket, true);
            let end = Instant::now() + Duration::from_secs(timeout);
            let address = match target.parse::<Addr>() {
                Ok(a) => a,
                Err(_) => {
                    c.ok(Request::Scan { on: true });
                    let found = loop {
                        match c.next(Some(end)) {
                            Some(Event::Found { address, name: Some(n), .. }) if n == target => break address,
                            Some(_) => {}
                            None => die(&format!("no device named {target:?} found")),
                        }
                    };
                    println!("found {target} at {found}");
                    found
                }
            };
            c.ok(Request::Pair { address });
            loop {
                match c.next(Some(end)) {
                    Some(Event::Prompt { address: a, prompt }) => answer(&mut c, a, prompt, yes),
                    Some(e @ Event::Paired { .. }) if matches!(e, Event::Paired { address: a, .. } if a == address) => {
                        print_event(&e);
                        return;
                    }
                    Some(e @ Event::PairingFailed { .. }) => {
                        print_event(&e);
                        std::process::exit(1);
                    }
                    Some(Event::Ok) => {}
                    Some(e) => print_event(&e),
                    None => die("pairing timed out"),
                }
            }
        }
        Some("agent") => {
            let mut c = Conn::open(&socket, true);
            println!("pairing agent ready");
            while let Some(e) = c.next(None) {
                match e {
                    Event::Prompt { address, prompt } => answer(&mut c, address, prompt, yes),
                    Event::Ok => {}
                    e => print_event(&e),
                }
            }
        }
        _ => die("usage: vinculum-ctl [-s SOCKET] status|devices|scan [SECS]|pair NAME|ADDR [--yes]|connect|disconnect|remove ADDR|agent [--yes]"),
    }
}
