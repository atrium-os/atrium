//! Drives Curia's Bluetooth page model (curia.md §14) against a real
//! vinculumd, as the page would with a person pressing its buttons: find a
//! device by name, pair it (answering the prompt as the person would),
//! disconnect, connect, forget after asking. Used by
//! `scripts/curia-bt-check.sh` (Rootcanal + a Bumble peer).
//!
//!   bt_drive SOCKET NAME [PASSKEY_FILE]
//!
//! Prints one `step: …` line per step and `bt-drive: ok` at the end; exits
//! non-zero, saying where, otherwise.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use curia::bluetooth::{label, BtPage};
use vinculum_control::{encode, Event, Prompt, Reader, Request};

struct Conn {
    s: UnixStream,
    r: Reader,
}

impl Conn {
    fn send(&mut self, p: &mut BtPage) {
        for w in p.take_wants() {
            self.s.write_all(&encode(&w)).expect("write");
        }
    }

    /// Events until `done(page)` holds, or `secs` pass.
    fn until(&mut self, p: &mut BtPage, secs: u64, what: &str, mut on: impl FnMut(&mut BtPage, &Event), done: impl Fn(&BtPage) -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut buf = [0u8; 4096];
        let mut asked = Instant::now();
        while !done(p) {
            // A controller still starting is asked again, as the page does.
            if !p.ready() && asked.elapsed() > Duration::from_secs(1) {
                asked = Instant::now();
                p.refresh();
                self.send(p);
            }
            if Instant::now() > end {
                eprintln!("bt-drive: FAIL waiting for {what}; note: {:?}", p.note);
                std::process::exit(1);
            }
            match self.s.read(&mut buf) {
                Ok(0) => {
                    eprintln!("bt-drive: FAIL vinculumd went away waiting for {what}");
                    std::process::exit(1);
                }
                Ok(n) => {
                    self.r.push(&buf[..n]);
                    while let Some(e) = self.r.next::<Event>().expect("wire") {
                        p.apply(e.clone());
                        on(p, &e);
                        self.send(p);
                    }
                }
                Err(_) => {}
            }
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (sock, name) = (&a[1], &a[2]);
    let passkey_file = a.get(3).cloned();
    let s = UnixStream::connect(sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut c = Conn { s, r: Reader::default() };
    c.s.write_all(&encode(&Request::Hello { agent: true })).unwrap();
    let mut p = BtPage::default();
    p.connected();
    c.send(&mut p);
    let nop = |_: &mut BtPage, _: &Event| {};
    c.until(&mut p, 20, "the controller", nop, |p| p.ready() && p.devices.is_some());
    println!("step: ready, {} paired", p.devices.as_ref().unwrap().len());

    p.toggle_scan();
    c.send(&mut p);
    let n = name.clone();
    c.until(&mut p, 30, "the device in the scan", nop, move |p| p.found.iter().any(|f| f.name.as_deref() == Some(&n)));
    let f = p.found.iter().find(|f| f.name.as_deref() == Some(name)).unwrap().clone();
    let addr = f.address;
    println!("step: found {} ({}{})", label(f.name.as_deref(), &addr), curia::bluetooth::found_line(&f), if f.class.is_some() { ", Classic" } else { "" });

    p.pair(addr);
    c.send(&mut p);
    // The prompt, answered as the person would on the page.
    let pf = passkey_file.clone();
    c.until(
        &mut p,
        60,
        "pairing",
        move |p, e| {
            if let Event::Prompt { prompt, .. } = e {
                match prompt {
                    Prompt::Confirm { number } => {
                        println!("step: prompt Confirm {number:?}: Pair");
                        p.confirm(true);
                    }
                    Prompt::EnterPasskey => {
                        let path = pf.as_deref().expect("a passkey file for this peer");
                        let end = Instant::now() + Duration::from_secs(10);
                        let code = loop {
                            if let Ok(t) = std::fs::read_to_string(path) {
                                if !t.trim().is_empty() {
                                    break t;
                                }
                            }
                            assert!(Instant::now() < end, "the peer showed no code");
                            std::thread::sleep(Duration::from_millis(100));
                        };
                        println!("step: prompt EnterPasskey: typed {}", code.trim());
                        p.passkey(Some(&code));
                    }
                    Prompt::Display { passkey } => {
                        println!("step: prompt Display {passkey:06}");
                        if let Some(path) = &pf {
                            std::fs::write(path, format!("{passkey:06}\n")).unwrap();
                        }
                        p.dismiss_display();
                    }
                }
            }
        },
        |p| p.pairing.is_none() && p.prompt.is_none() && p.note.is_some(),
    );
    println!("step: {}", p.note.clone().unwrap());
    if !p.note.as_deref().unwrap().starts_with("Paired with") {
        std::process::exit(1);
    }
    c.until(&mut p, 20, "the paired list", nop, move |p| p.devices.as_ref().is_some_and(|v| v.iter().any(|d| d.address == addr || d.identity == Some(addr))));
    let d = p.devices.as_ref().unwrap().iter().find(|d| d.address == addr || d.identity == Some(addr)).unwrap().clone();
    println!("step: paired list: {} · {}", d.name, curia::bluetooth::device_line(&d));
    p.toggle_scan();
    c.send(&mut p);

    if d.connected {
        p.disconnect(d.address);
        c.send(&mut p);
        let da = d.address;
        c.until(&mut p, 20, "disconnect", nop, move |p| p.devices.as_ref().is_some_and(|v| v.iter().any(|x| x.address == da && !x.connected)));
        println!("step: disconnected");
    }
    p.connect(d.address);
    c.send(&mut p);
    let da = d.address;
    c.until(&mut p, 30, "connect", nop, move |p| p.devices.as_ref().is_some_and(|v| v.iter().any(|x| x.address == da && x.connected)));
    println!("step: connected again (with the stored bond)");

    // Turn Bluetooth off: the link drops and nothing connects; then on.
    p.power(false);
    c.send(&mut p);
    c.until(&mut p, 20, "Bluetooth off", nop, move |p| {
        p.status.as_ref().is_some_and(|s| !s.powered) && p.devices.as_ref().is_some_and(|v| v.iter().any(|x| x.address == da && !x.connected))
    });
    p.connect(da);
    assert!(p.take_wants().is_empty(), "no Connect while off");
    println!("step: turned off, the device dropped");
    p.power(true);
    c.send(&mut p);
    c.until(&mut p, 20, "Bluetooth on", nop, |p| p.on());
    p.connect(da);
    c.send(&mut p);
    c.until(&mut p, 30, "connect after turning on", nop, move |p| p.devices.as_ref().is_some_and(|v| v.iter().any(|x| x.address == da && x.connected)));
    println!("step: turned on, connected again");

    p.forget(d.address);
    c.send(&mut p);
    assert_eq!(p.confirm_forget, Some(d.address), "forget asks first");
    p.forget(d.address);
    c.send(&mut p);
    c.until(&mut p, 20, "forget", nop, move |p| p.devices.as_ref().is_some_and(|v| !v.iter().any(|x| x.address == da)));
    println!("step: forgotten");
    println!("bt-drive: ok");
}
