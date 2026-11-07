//! vinculum-probe: a dev tool that drives a Bluetooth controller over H4
//! and prints the parsed HCI traffic. It is the B0 harness host side
//! (docs/spec/bluetooth.md §14, §15): against Rootcanal it proves the
//! generated parsers read what a real controller and a Bumble peer produce,
//! and `--record` keeps the traffic as a btsnoop trace for host tests.
//!
//! ```text
//! vinculum-probe --tcp 127.0.0.1:6402 [--scan SECS] [--expect NAME] [--record FILE]
//! ```
//!
//! Sequence: Reset, Read_BD_ADDR, Read_Local_Version_Information,
//! Set_Event_Mask (LE meta on), LE_Set_Scan_Parameters (active),
//! LE_Set_Scan_Enable, then advertising reports for SECS seconds.
//! Exit status 0 when every command completed and, with `--expect`, a device
//! advertising that name was seen.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vinculum_hci_proto::btsnoop::{self, Direction, Record};
use vinculum_hci_proto::h4::{Decoder, Frame, Kind};
use vinculum_hci_proto::hci::{self, Command, Event, EventChild, LeMetaEventChild};
use vinculum_hci_proto::{gap, Packet};

struct Opts {
    tcp: String,
    scan_secs: u64,
    expect: Option<String>,
    record: Option<String>,
}

fn usage() -> ! {
    eprintln!("usage: vinculum-probe --tcp HOST:PORT [--scan SECS] [--expect NAME] [--record FILE]");
    std::process::exit(2)
}

fn opts() -> Opts {
    let mut o = Opts { tcp: String::new(), scan_secs: 3, expect: None, record: None };
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        let mut v = || a.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--tcp" => o.tcp = v(),
            "--scan" => o.scan_secs = v().parse().unwrap_or_else(|_| usage()),
            "--expect" => o.expect = Some(v()),
            "--record" => o.record = Some(v()),
            _ => usage(),
        }
    }
    if o.tcp.is_empty() {
        usage()
    }
    o
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

fn fmt_addr(a: hci::Address) -> String {
    let v: u64 = a.into();
    let b = v.to_le_bytes();
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[5], b[4], b[3], b[2], b[1], b[0])
}

struct Link {
    sock: TcpStream,
    dec: Decoder,
    log: Option<btsnoop::Writer<std::fs::File>>,
    pending: Vec<Frame>,
}

impl Link {
    fn note(&mut self, d: Direction, f: &Frame) {
        if let Some(w) = self.log.as_mut() {
            w.write(&Record { direction: d, unix_us: now_us(), frame: f.clone() }).expect("btsnoop write");
        }
    }

    fn send(&mut self, cmd: Command) {
        let f = Frame { kind: Kind::Command, packet: cmd.encode_to_vec().expect("encode") };
        self.note(Direction::HostToController, &f);
        self.sock.write_all(&f.to_bytes()).expect("send");
    }

    /// Next frame from the controller, or None after `until`.
    fn recv(&mut self, until: Instant) -> Option<Frame> {
        loop {
            if !self.pending.is_empty() {
                let f = self.pending.remove(0);
                self.note(Direction::ControllerToHost, &f);
                return Some(f);
            }
            let left = until.checked_duration_since(Instant::now())?;
            self.sock.set_read_timeout(Some(left.max(Duration::from_millis(1)))).ok();
            let mut buf = [0u8; 4096];
            match self.sock.read(&mut buf) {
                Ok(0) => panic!("controller closed the connection"),
                Ok(n) => {
                    let pending = &mut self.pending;
                    self.dec.push(&buf[..n], |f| pending.push(f)).expect("H4 stream");
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    return None
                }
                Err(e) => panic!("read: {e}"),
            }
        }
    }
}

#[derive(Default)]
struct Seen {
    events: BTreeMap<String, u32>,
    /// address -> (name, rssi)
    devices: BTreeMap<String, (Option<String>, i8)>,
    parse_errors: u32,
}

/// Decodes one controller frame fully, recording what it was. Returns the
/// Command Complete/Status opcode, if any.
fn decode(f: &Frame, seen: &mut Seen) -> Option<(hci::OpCode, hci::ErrorCode)> {
    if f.kind != Kind::Event {
        *seen.events.entry(format!("{:?}", f.kind)).or_default() += 1;
        return None;
    }
    let ev = match Event::decode_full(&f.packet) {
        Ok(e) => e,
        Err(e) => {
            seen.parse_errors += 1;
            eprintln!("event parse error: {e:?} in {:02x?}", f.packet);
            return None;
        }
    };
    let child = match ev.specialize() {
        Ok(c) => c,
        Err(e) => {
            seen.parse_errors += 1;
            eprintln!("event {:?} specialize error: {e:?}", ev.event_code);
            return None;
        }
    };
    let name = format!("{:?}", ev.event_code);
    *seen.events.entry(name).or_default() += 1;
    match child {
        EventChild::CommandComplete(cc) => {
            let status = cc.payload.first().copied().unwrap_or(0);
            let status = hci::ErrorCode::try_from(status).unwrap_or(hci::ErrorCode::StatusUnknown);
            match cc.command_op_code {
                hci::OpCode::ReadBdAddr => match hci::ReadBdAddrComplete::try_from(&cc) {
                    Ok(r) => println!("controller address {}", fmt_addr(r.bd_addr)),
                    Err(e) => {
                        seen.parse_errors += 1;
                        eprintln!("ReadBdAddrComplete: {e:?}")
                    }
                },
                hci::OpCode::ReadLocalVersionInformation => {
                    match hci::ReadLocalVersionInformationComplete::try_from(&cc) {
                        Ok(r) => println!("controller version {:?}", r.local_version_information),
                        Err(e) => {
                            seen.parse_errors += 1;
                            eprintln!("ReadLocalVersionInformationComplete: {e:?}")
                        }
                    }
                }
                _ => {}
            }
            Some((cc.command_op_code, status))
        }
        EventChild::CommandStatus(cs) => Some((cs.command_op_code, cs.status)),
        EventChild::LeMetaEvent(m) => {
            match m.specialize() {
                Ok(LeMetaEventChild::LeAdvertisingReport(r)) => {
                    for resp in r.responses {
                        note_device(seen, resp.address, &resp.advertising_data, resp.rssi);
                    }
                }
                Ok(LeMetaEventChild::LeExtendedAdvertisingReport(r)) => {
                    for resp in r.responses {
                        note_device(seen, resp.address, &resp.advertising_data, resp.rssi);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    seen.parse_errors += 1;
                    eprintln!("LE meta {:?}: {e:?}", m.subevent_code);
                }
            }
            None
        }
        _ => None,
    }
}

fn note_device(seen: &mut Seen, addr: hci::Address, data: &[u8], rssi: u8) {
    let name = match gap::parse(data) {
        Ok(s) => gap::local_name(&s),
        Err(e) => {
            seen.parse_errors += 1;
            eprintln!("advertising data from {}: {e:?}", fmt_addr(addr));
            None
        }
    };
    let e = seen.devices.entry(fmt_addr(addr)).or_insert((None, rssi as i8));
    if name.is_some() {
        e.0 = name;
    }
    e.1 = rssi as i8;
}

fn run(link: &mut Link, seen: &mut Seen, cmd: Command) -> bool {
    let op = cmd.op_code;
    link.send(cmd);
    let until = Instant::now() + Duration::from_secs(2);
    while let Some(f) = link.recv(until) {
        if let Some((done, status)) = decode(&f, seen) {
            if done == op {
                println!("{op:?}: {status:?}");
                return status == hci::ErrorCode::Success;
            }
        }
    }
    eprintln!("{op:?}: no completion within 2 s");
    false
}

fn main() {
    let o = opts();
    let sock = TcpStream::connect(&o.tcp).unwrap_or_else(|e| {
        eprintln!("connect {}: {e}", o.tcp);
        std::process::exit(1)
    });
    sock.set_nodelay(true).ok();
    let log = o.record.as_ref().map(|p| btsnoop::Writer::new(std::fs::File::create(p).expect("record file")).unwrap());
    let mut link = Link { sock, dec: Decoder::new(), log, pending: Vec::new() };
    let mut seen = Seen::default();

    let cmds: Vec<Command> = vec![
        hci::Reset {}.try_into().unwrap(),
        hci::ReadBdAddr {}.try_into().unwrap(),
        hci::ReadLocalVersionInformation {}.try_into().unwrap(),
        // Default mask plus LE Meta (bit 61).
        hci::SetEventMask { event_mask: 0x2000_1FFF_FFFF_FFFF }.try_into().unwrap(),
        hci::LeSetScanParameters {
            le_scan_type: hci::LeScanType::Active,
            le_scan_interval: 0x0010,
            le_scan_window: 0x0010,
            own_address_type: hci::OwnAddressType::PublicDeviceAddress,
            scanning_filter_policy: hci::LeScanningFilterPolicy::AcceptAll,
        }
        .try_into()
        .unwrap(),
        hci::LeSetScanEnable { le_scan_enable: hci::Enable::Enabled, filter_duplicates: hci::Enable::Disabled }
            .try_into()
            .unwrap(),
    ];
    let mut ok = true;
    for c in cmds {
        ok &= run(&mut link, &mut seen, c);
    }
    let until = Instant::now() + Duration::from_secs(o.scan_secs);
    while let Some(f) = link.recv(until) {
        decode(&f, &mut seen);
    }
    ok &= run(
        &mut link,
        &mut seen,
        hci::LeSetScanEnable { le_scan_enable: hci::Enable::Disabled, filter_duplicates: hci::Enable::Disabled }
            .try_into()
            .unwrap(),
    );

    println!("events: {:?}", seen.events);
    for (a, (n, rssi)) in &seen.devices {
        println!("device {a} rssi {rssi} name {}", n.as_deref().unwrap_or("-"));
    }
    if seen.parse_errors > 0 {
        eprintln!("{} parse errors", seen.parse_errors);
        ok = false;
    }
    if let Some(want) = &o.expect {
        if !seen.devices.values().any(|(n, _)| n.as_deref() == Some(want.as_str())) {
            eprintln!("no device advertising {want:?} seen");
            ok = false;
        }
    }
    std::process::exit(if ok { 0 } else { 1 })
}
