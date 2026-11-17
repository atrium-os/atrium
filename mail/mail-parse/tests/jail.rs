//! The parse jail end to end: the real binary, driven the way the engine
//! drives it (mail.md §4, §7.1), with every frame it sends checked as the
//! engine checks them.
#![cfg(feature = "jail")]

use mail_schema::{self as wire, Caps, FromJail, ToJail};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::io::{BufReader, BufWriter, Write};
use std::process::{Command, Stdio};

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 { if i <= c.len() { s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char) } else { s.push('=') } }
    }
    s
}

/// An Ed25519-signed message (simple/simple, so the signed bytes are literal).
fn signed(kp: &Ed25519KeyPair, headers: &str, body: &str) -> Vec<u8> {
    let bh = b64(ring::digest::digest(&ring::digest::SHA256, body.as_bytes()).as_ref());
    let field = format!("DKIM-Signature: v=1; a=ed25519-sha256; c=simple/simple; d=example.com; s=sel; h=from:subject:content-type; bh={bh}; b=");
    let mut data = headers.as_bytes().to_vec();
    data.extend_from_slice(field.as_bytes());
    let sig = kp.sign(ring::digest::digest(&ring::digest::SHA256, &data).as_ref());
    format!("{field}{}\r\n{headers}\r\n{body}", b64(sig.as_ref())).into_bytes()
}

const HEADERS: &str = "From: Priya Nair <priya@example.com>\r\nSubject: Autumn sale\r\nContent-Type: text/html; charset=utf-8\r\n";
const BODY: &str = "<p>Hello <a href=\"https://shop.example/x\">sale</a></p>\r\n<img src=\"https://cdn.example/coat.jpg\" width=\"300\" height=\"200\">\r\n<img src=\"https://track.example/o.gif\" width=\"1\" height=\"1\">\r\n";

struct Engine { child: std::process::Child, r: BufReader<std::process::ChildStdout>, w: BufWriter<std::process::ChildStdin>, queries: Vec<String> }

impl Engine {
    fn start() -> Engine {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mail-parse-jail")).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let r = BufReader::new(child.stdout.take().unwrap());
        let w = BufWriter::new(child.stdin.take().unwrap());
        Engine { child, r, w, queries: Vec::new() }
    }
    fn send(&mut self, m: &ToJail) { wire::write_frame(&mut self.w, m).unwrap() }
    /// Read frames until `Done`, answering queries from `dns`.
    fn run(&mut self, raw_len: u64, dns: &dyn Fn(&str) -> Option<String>) -> Vec<FromJail> {
        let mut out = Vec::new();
        loop {
            let f: FromJail = wire::read_frame(&mut self.r).unwrap().expect("frame");
            f.check(&Caps::default(), raw_len).expect("the engine accepts it");
            match f {
                FromJail::Query { query, name } => {
                    self.queries.push(name.clone());
                    let txt = dns(&name);
                    self.send(&ToJail::Answer { query, txt });
                }
                FromJail::Done => return out,
                other => out.push(other),
            }
        }
    }
    fn finish(mut self) {
        drop(self.w);
        assert!(self.child.wait().unwrap().success());
        let _ = self.r.get_mut();
    }
}

#[test]
fn a_batch_is_summarized_with_keys_from_the_engine() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let rec = format!("v=DKIM1; k=ed25519; p={}", b64(kp.public_key().as_ref()));
    let good = signed(&kp, HEADERS, BODY);
    let forged = String::from_utf8(good.clone()).unwrap().replace("Autumn sale", "Wire the money").into_bytes();
    let plain = b"From: a@b.example\r\nSubject: hi\r\n\r\nhello\r\n".to_vec();
    let not_mail = b"\x00\x01garbage".to_vec();

    let mut e = Engine::start();
    let batch: Vec<wire::Raw> = [&good, &forged, &plain, &not_mail].iter().enumerate()
        .map(|(i, b)| wire::Raw { id: i as u64, bytes: b.to_vec() }).collect();
    let max_len = batch.iter().map(|m| m.bytes.len()).max().unwrap() as u64;
    e.send(&ToJail::Summarize { authserv_id: None, me: vec![], now: 0, headers_only: false, batch });
    let dns = |n: &str| match n { "sel._domainkey.example.com" => Some(rec.clone()), _ => None };
    let out = e.run(max_len, &dns);
    assert_eq!(out.len(), 4);
    let check = |i: usize| match &out[i] { FromJail::Summary { id, result: Ok(s) } if *id == i as u64 => s.sender_check.clone(), o => panic!("{o:?}") };
    assert_eq!(check(0), wire::SenderCheck::Verified { domain: "example.com".into(), by_provider: false });
    assert_eq!(check(1), wire::SenderCheck::NotVerified, "a changed subject breaks the signature");
    assert_eq!(check(2), wire::SenderCheck::NotVerified);
    assert!(matches!(&out[3], FromJail::Summary { id: 3, result: Err(_) }), "{:?}", out[3]);
    // Only key and DMARC lookups, and the unsigned message asked for none.
    assert!(e.queries.iter().all(|q| q == "sel._domainkey.example.com" || q == "_dmarc.example.com"), "{:?}", e.queries);
    e.finish();
}

#[test]
fn opening_converts_the_html_and_drops_the_pixel() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let raw = signed(&kp, HEADERS, BODY);
    let mut e = Engine::start();
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, message: wire::Raw { id: 7, bytes: raw.clone() }, loaded: vec![] });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Opened { id: 7, result: Ok(o) } = &out[0] else { panic!("{out:?}") };
    assert_eq!(o.summary.sender_check, wire::SenderCheck::Pending, "no key could be fetched");
    let d = o.document.as_ref().expect("an HTML document");
    assert!(d.html.contains("Hello") && !d.html.contains("cdn.example") && !d.html.contains("track.example"));
    let loadable: Vec<_> = d.resources.iter().filter(|r| r.loadable()).collect();
    assert_eq!(loadable.len(), 1, "{:?}", d.resources);
    assert!(d.resources.iter().any(|r| r.hidden && r.source == wire::Source::Remote("https://track.example/o.gif".into())));
    e.finish();
}

#[test]
fn the_engine_refuses_a_lying_frame() {
    let lie = FromJail::Query { query: 0, name: "evil.example".into() };
    assert!(lie.check(&Caps::default(), 0).is_err());
    let mut s = wire::Summary { from: vec![wire::Address { addr: "x@bank.example".into(), ..Default::default() }], ..Default::default() };
    s.sender_check = wire::SenderCheck::Verified { domain: "evil.example".into(), by_provider: false };
    assert!(s.check(&Caps::default(), 10).is_err(), "verified domain must be From's");
    s.sender_check = wire::SenderCheck::NotVerified;
    s.parts.push(wire::Part { path: "1".into(), kind: wire::PartKind::Text, content_type: "text/plain".into(), filename: None, content_id: None, size: 1, body: (0, 99) });
    assert!(s.check(&Caps::default(), 10).is_err(), "part outside the message");
    assert!(wire::query_allowed("sel._domainkey.example.com") && wire::query_allowed("_dmarc.example.com"));
    assert!(!wire::query_allowed("_dmarc.com") && !wire::query_allowed("a.example.com") && !wire::query_allowed("x._domainkey.a b.com"));
    // A frame over the limit is refused before it is read.
    let mut big = Vec::new();
    big.extend_from_slice(&((wire::MAX_FRAME as u32) + 1).to_be_bytes());
    assert!(wire::read_frame::<FromJail>(&mut big.as_slice()).is_err());
    let _ = std::io::sink().flush();
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(png::ColorType::Rgb);
    e.write_header().unwrap().write_image_data(&vec![120u8; (w * h * 3) as usize]).unwrap();
    out
}

#[test]
fn a_message_s_own_pictures_come_with_its_document() {
    let logo = png(120, 40);
    let dot = png(2, 2);
    let raw = format!(
        "From: Shop <news@shop.example>\r\nTo: p@example.org\r\nSubject: New\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi</p><img src=\"cid:logo@shop\"><img src=\"data:image/png;base64,{}\" width=\"2\" height=\"2\"><img src=\"cid:missing@shop\" width=\"10\" height=\"10\"><img src=\"https://cdn.example/a.png\" width=\"50\" height=\"50\">\r\n\
--b\r\nContent-Type: image/png\r\nContent-ID: <logo@shop>\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--b--\r\n",
        b64(&dot), b64(&logo)).into_bytes();
    let mut e = Engine::start();
    e.send(&ToJail::Open { authserv_id: None, me: vec![], now: 0, message: wire::Raw { id: 9, bytes: raw.clone() }, loaded: vec![] });
    let out = e.run(raw.len() as u64, &|_| None);
    let FromJail::Opened { id: 9, result: Ok(o) } = &out[0] else { panic!("{out:?}") };
    let d = o.document.as_ref().unwrap();
    let id_of = |src: &wire::Source| d.resources.iter().find(|r| r.source == *src).map(|r| r.id.clone()).unwrap();
    let logo_id = id_of(&wire::Source::Cid("logo@shop".into()));
    let got: Vec<&str> = d.pictures.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(got.len(), 2, "the cid part and the data URL, not the missing part or the remote one: {got:?}");
    assert_eq!(d.pictures.iter().find(|(i, _)| *i == logo_id).unwrap().1, logo);
    // The logo declared no size: its real one is in the document.
    assert!(d.html.contains(&format!("src=\"{logo_id}\" width=\"120\" height=\"40\"")) || d.html.contains(&format!("src=\"{logo_id}\" class")), "{}", d.html);
    assert!(d.html.contains("width=\"120\""), "{}", d.html);
    e.finish();
}
