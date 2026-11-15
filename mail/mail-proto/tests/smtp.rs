use mail_proto::smtp::*;

fn reply(d: &mut ReplyDecoder, s: &str) -> Reply {
    d.feed(s.as_bytes());
    d.next().unwrap().unwrap()
}

#[test]
fn ehlo_caps() {
    let mut d = ReplyDecoder::default();
    let r = reply(&mut d, "250-smtp.example.org at your service\r\n250-SIZE 35882577\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-PIPELINING\r\n250 FUTURERELEASE 604800 2026-12-31T00:00:00Z\r\n");
    let c = Caps::from_ehlo(&r);
    assert_eq!(c, Caps { size: Some(35882577), eightbit: true, smtputf8: true, pipelining: true, chunking: false, future_release: Some(604800) });
}

#[test]
fn send_with_one_rejected_recipient() {
    let caps = Caps { size: Some(1000), eightbit: true, ..Caps::default() };
    let mut s = Submission::new(&caps, "me@x.example", &["a@y.example", "b@z.example"], b"Subject: hi\n\n.leading dot\nend", None).unwrap();
    assert_eq!(s.start(), b"MAIL FROM:<me@x.example> SIZE=29\r\n");
    let mut d = ReplyDecoder::default();
    assert_eq!(s.on_reply(reply(&mut d, "250 ok\r\n")), Step::Send(b"RCPT TO:<a@y.example>\r\n".to_vec()));
    assert_eq!(s.on_reply(reply(&mut d, "550 5.1.1 no such user\r\n")), Step::Send(b"RCPT TO:<b@z.example>\r\n".to_vec()));
    assert_eq!(s.on_reply(reply(&mut d, "250 ok\r\n")), Step::Send(b"DATA\r\n".to_vec()));
    assert_eq!(s.on_reply(reply(&mut d, "354 go\r\n")), Step::Send(b"Subject: hi\r\n\r\n..leading dot\r\nend\r\n.\r\n".to_vec()));
    match s.on_reply(reply(&mut d, "250 2.0.0 queued as X\r\n")) {
        Step::Sent { rejected } => assert_eq!(rejected[0].0, "a@y.example"),
        x => panic!("{x:?}"),
    }
}

#[test]
fn all_rejected_resets() {
    let mut s = Submission::new(&Caps::default(), "me@x.example", &["a@y.example"], b"x", None).unwrap();
    s.start();
    let mut d = ReplyDecoder::default();
    s.on_reply(reply(&mut d, "250 ok\r\n"));
    assert_eq!(s.on_reply(reply(&mut d, "450 4.2.0 try later\r\n")), Step::Send(b"RSET\r\n".to_vec()));
    match s.on_reply(reply(&mut d, "250 reset\r\n")) {
        Step::Failed { stage: "RCPT TO", transient: true, .. } => {}
        x => panic!("{x:?}"),
    }
}

#[test]
fn refusals() {
    let c = Caps::default();
    assert_eq!(Submission::new(&c, "me@x.example", &["a@y.example>\r\nRCPT TO:<z@evil"], b"x", None).err(), Some(Refusal::BadAddress("a@y.example>\r\nRCPT TO:<z@evil".into())));
    assert!(matches!(Submission::new(&c, "me@x.example", &["zoë@y.example"], b"x", None), Err(Refusal::NeedsSmtpUtf8(_))));
    assert!(matches!(Submission::new(&Caps { size: Some(1), ..c.clone() }, "me@x.example", &["a@y.example"], b"xx", None), Err(Refusal::TooBig { .. })));
    assert_eq!(Submission::new(&c, "me@x.example", &["a@y.example"], b"x", Some(60)).err(), Some(Refusal::HoldNotOffered));
    let mut s = Submission::new(&Caps { future_release: Some(3600), smtputf8: true, ..c }, "", &["zoë@y.example"], b"x", Some(60)).unwrap();
    assert_eq!(s.start(), "MAIL FROM:<> SMTPUTF8 HOLDFOR=60\r\n".as_bytes());
}

#[test]
fn reply_decoder_rejects_junk() {
    let mut d = ReplyDecoder::default();
    d.feed(b"250-a\r\n251 b\r\n");
    assert_eq!(d.next(), Some(Err(Error::Syntax)));
    let mut d = ReplyDecoder::default();
    d.feed(&[b'2'; 5000]);
    assert_eq!(d.next(), Some(Err(Error::TooLong)));
}

#[test]
fn dot_stuffing() {
    assert_eq!(dot_stuff(b".\r\n..\n\r.x"), b"..\r\n...\r\n\r\n..x\r\n.\r\n");
    assert_eq!(dot_stuff(b""), b".\r\n");
}
