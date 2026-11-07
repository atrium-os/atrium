//! HOGP discovery against a small in-test GATT server.

use vinculum_gatt::{op, prop, uuid, Hogp, Out};

struct Attr {
    handle: u16,
    ty: u16,
    value: Vec<u8>,
}

struct Server {
    attrs: Vec<Attr>,
    mtu: usize,
    writes: Vec<(u16, Vec<u8>)>,
}

fn err(req: u8, h: u16, code: u8) -> Vec<u8> {
    let mut v = vec![op::ERROR_RSP, req];
    v.extend_from_slice(&h.to_le_bytes());
    v.push(code);
    v
}

impl Server {
    /// A mouse: HID (Report Map, one input report with id `id`, one output
    /// report), Device Information (PnP ID), Battery (notify).
    fn mouse(map: Vec<u8>, id: u8) -> Server {
        let mut attrs = Vec::new();
        let mut h = 0u16;
        let mut add = |ty: u16, value: Vec<u8>| {
            h += 1;
            attrs.push(Attr { handle: h, ty, value });
            h
        };
        let svc = |u: u16| u.to_le_bytes().to_vec();
        // Characteristic declaration: props, value handle (next), uuid.
        fn decl(props: u8, value: u16, u: u16) -> Vec<u8> {
            let mut v = vec![props];
            v.extend_from_slice(&value.to_le_bytes());
            v.extend_from_slice(&u.to_le_bytes());
            v
        }
        // GAP service, which we ignore.
        add(uuid::PRIMARY_SERVICE, svc(0x1800));
        add(uuid::CHARACTERISTIC, decl(prop::READ, 3, 0x2A00));
        add(0x2A00, b"Mouse".to_vec());
        // HID.
        add(uuid::PRIMARY_SERVICE, svc(uuid::HID)); // 4
        add(uuid::CHARACTERISTIC, decl(prop::READ, 6, uuid::REPORT_MAP));
        add(uuid::REPORT_MAP, map); // 6
        add(uuid::CHARACTERISTIC, decl(prop::READ | prop::NOTIFY, 8, uuid::REPORT));
        add(uuid::REPORT, vec![0, 0, 0]); // 8
        add(uuid::CCCD, vec![0, 0]); // 9
        add(uuid::REPORT_REFERENCE, vec![id, 1]); // 10
        add(uuid::CHARACTERISTIC, decl(prop::READ | prop::WRITE | prop::WRITE_NO_RSP, 12, uuid::REPORT));
        add(uuid::REPORT, vec![0]); // 12
        add(uuid::REPORT_REFERENCE, vec![2, 2]); // 13
        // Device Information.
        add(uuid::PRIMARY_SERVICE, svc(uuid::DEVICE_INFO)); // 14
        add(uuid::CHARACTERISTIC, decl(prop::READ, 16, uuid::PNP_ID));
        add(uuid::PNP_ID, vec![2, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01]); // 16
        // Battery.
        add(uuid::PRIMARY_SERVICE, svc(uuid::BATTERY)); // 17
        add(uuid::CHARACTERISTIC, decl(prop::READ | prop::NOTIFY, 19, uuid::BATTERY_LEVEL));
        add(uuid::BATTERY_LEVEL, vec![87]); // 19
        add(uuid::CCCD, vec![0, 0]); // 20
        Server { attrs, mtu: 23, writes: Vec::new() }
    }

    fn group_end(&self, i: usize) -> u16 {
        self.attrs[i + 1..].iter().find(|a| a.ty == uuid::PRIMARY_SERVICE).map_or(0xFFFF, |a| a.handle - 1)
    }

    fn answer(&mut self, p: &[u8]) -> Vec<u8> {
        let u = |o: usize| u16::from_le_bytes([p[o], p[o + 1]]);
        match p[0] {
            op::MTU_REQ => {
                self.mtu = (u(1) as usize).min(64);
                let mut v = vec![op::MTU_RSP];
                v.extend_from_slice(&64u16.to_le_bytes());
                v
            }
            op::READ_BY_GROUP_REQ => {
                let (s, e) = (u(1), u(3));
                let mut v = vec![op::READ_BY_GROUP_RSP, 6];
                for (i, a) in self.attrs.iter().enumerate() {
                    if a.handle >= s && a.handle <= e && a.ty == uuid::PRIMARY_SERVICE && v.len() + 6 <= self.mtu {
                        v.extend_from_slice(&a.handle.to_le_bytes());
                        v.extend_from_slice(&self.group_end(i).to_le_bytes());
                        v.extend_from_slice(&a.value);
                    }
                }
                if v.len() == 2 { err(p[0], s, 0x0A) } else { v }
            }
            op::READ_BY_TYPE_REQ => {
                let (s, e, t) = (u(1), u(3), u(5));
                let mut v = vec![op::READ_BY_TYPE_RSP, 7];
                for a in &self.attrs {
                    if a.handle >= s && a.handle <= e && a.ty == t && v.len() + 7 <= self.mtu {
                        v.extend_from_slice(&a.handle.to_le_bytes());
                        v.extend_from_slice(&a.value);
                    }
                }
                if v.len() == 2 { err(p[0], s, 0x0A) } else { v }
            }
            op::FIND_INFO_REQ => {
                let (s, e) = (u(1), u(3));
                let mut v = vec![op::FIND_INFO_RSP, 1];
                for a in &self.attrs {
                    if a.handle >= s && a.handle <= e && v.len() + 4 <= self.mtu {
                        v.extend_from_slice(&a.handle.to_le_bytes());
                        v.extend_from_slice(&a.ty.to_le_bytes());
                    }
                }
                if v.len() == 2 { err(p[0], s, 0x0A) } else { v }
            }
            op::READ_REQ | op::READ_BLOB_REQ => {
                let h = u(1);
                let off = if p[0] == op::READ_BLOB_REQ { u(3) as usize } else { 0 };
                let Some(a) = self.attrs.iter().find(|a| a.handle == h) else { return err(p[0], h, 0x01) };
                if off > a.value.len() {
                    return err(p[0], h, 0x07);
                }
                let mut v = vec![if off == 0 { op::READ_RSP } else { op::READ_BLOB_RSP }];
                let end = (off + self.mtu - 1).min(a.value.len());
                v.extend_from_slice(&a.value[off..end]);
                v
            }
            op::WRITE_REQ => {
                self.writes.push((u(1), p[3..].to_vec()));
                vec![op::WRITE_RSP]
            }
            _ => err(p[0], 0, 0x06),
        }
    }
}

/// Runs discovery to completion; returns the Ready device and the client.
fn run(s: &mut Server) -> (Vec<Out>, Hogp) {
    let mut c = Hogp::new();
    let mut pending: Vec<Out> = c.start(0);
    let mut done = Vec::new();
    for _ in 0..500 {
        let Some(o) = pending.pop() else { break };
        match o {
            Out::Send(p) => pending.extend(c.on_pdu(&s.answer(&p), 0)),
            other => done.push(other),
        }
    }
    (done, c)
}

const MOUSE_MAP: [u8; 52] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
    0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xC0, 0xC0,
];

#[test]
fn discovers_a_mouse() {
    let mut s = Server::mouse(MOUSE_MAP.to_vec(), 1);
    let (done, c) = run(&mut s);
    let Some(Out::Ready(d)) = done.first() else { panic!("not ready: {done:?}") };
    // 52 bytes at MTU 64 needs no blob read; a longer map is tested below.
    assert_eq!(d.report_map, MOUSE_MAP);
    assert_eq!((d.vendor, d.product, d.version), (0x1234, 0x5678, 0x0100));
    assert_eq!(d.battery, Some(87));
    assert_eq!(d.inputs, vec![(8, 1)]);
    assert_eq!(d.outputs, vec![(2, 12)]);
    // Notifications on: input report CCCD (9) and battery CCCD (20).
    assert!(s.writes.contains(&(9, vec![1, 0])) && s.writes.contains(&(20, vec![1, 0])));
    assert!(c.is_ready());
}

#[test]
fn reports_battery_and_output() {
    let mut s = Server::mouse(MOUSE_MAP.to_vec(), 1);
    let (_, mut c) = run(&mut s);
    assert_eq!(c.on_pdu(&[op::NOTIFY, 8, 0, 1, 5, 0xFB], 0), vec![Out::Report { id: 1, data: vec![1, 5, 0xFB] }]);
    assert_eq!(c.on_pdu(&[op::NOTIFY, 19, 0, 40], 0), vec![Out::Battery(40)]);
    // Unknown handles are ignored; indications are confirmed.
    assert_eq!(c.on_pdu(&[op::NOTIFY, 99, 0, 1], 0), vec![]);
    assert_eq!(c.on_pdu(&[op::INDICATE, 99, 0, 1], 0), vec![Out::Send(vec![op::CONFIRM])]);
    assert_eq!(c.output(2, &[1]), Some(vec![op::WRITE_CMD, 12, 0, 1]));
    assert_eq!(c.output(3, &[1]), None);
}

#[test]
fn long_report_map_uses_read_blob() {
    let map: Vec<u8> = MOUSE_MAP.iter().copied().cycle().take(300).collect();
    let mut s = Server::mouse(map.clone(), 1);
    let (done, _) = run(&mut s);
    let Some(Out::Ready(d)) = done.first() else { panic!("not ready: {done:?}") };
    assert_eq!(d.report_map, map);
}

#[test]
fn refuses_oversized_map_and_non_hid() {
    let mut s = Server::mouse(vec![0x05; 600], 1);
    let (done, _) = run(&mut s);
    assert!(matches!(done.first(), Some(Out::Failed(w)) if w.contains("512")), "{done:?}");

    let mut s = Server::mouse(MOUSE_MAP.to_vec(), 1);
    s.attrs.retain(|a| a.handle < 4 || a.handle > 13);
    let (done, _) = run(&mut s);
    assert!(matches!(done.first(), Some(Out::Failed(w)) if w.contains("no HID")), "{done:?}");
}

#[test]
fn answers_requests_to_our_server_and_rejects_stray_responses() {
    let mut c = Hogp::new();
    let first = c.start(0);
    assert_eq!(first, vec![Out::Send(vec![op::MTU_REQ, 247, 0])]);
    // The peer discovers our services: nothing here.
    assert_eq!(c.on_pdu(&[op::READ_BY_GROUP_REQ, 1, 0, 0xFF, 0xFF, 0x00, 0x28], 0), vec![Out::Send(vec![op::ERROR_RSP, 0x10, 1, 0, 0x0A])]);
    // Its own MTU request is answered.
    assert_eq!(c.on_pdu(&[op::MTU_REQ, 100, 0], 0), vec![Out::Send(vec![op::MTU_RSP, 247, 0])]);
    // Write commands get nothing.
    assert_eq!(c.on_pdu(&[op::WRITE_CMD, 1, 0, 1], 0), vec![]);
    // Our MTU request answered, then a response nobody asked for.
    let o = c.on_pdu(&[op::MTU_RSP, 64, 0], 0);
    assert!(matches!(o.as_slice(), [Out::Send(p)] if p[0] == op::READ_BY_GROUP_REQ));
    let _ = c.on_pdu(&[op::ERROR_RSP, 0x10, 1, 0, 0x0A], 0); // no services: fails (no HID)
    let o = c.on_pdu(&[op::READ_RSP, 1], 0);
    assert!(o.is_empty() || matches!(o.as_slice(), [Out::Failed(_)]));
}

#[test]
fn deadline_tracks_the_request_in_flight() {
    let mut c = Hogp::new();
    assert_eq!(c.deadline(), None);
    let _ = c.start(1000);
    assert_eq!(c.deadline(), Some(1000 + vinculum_gatt::ATT_TIMEOUT_MS));
}
