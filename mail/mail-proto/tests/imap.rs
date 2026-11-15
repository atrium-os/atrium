use mail_proto::imap::*;

fn all(input: &[u8], step: usize) -> Vec<Response> {
    let mut d = Decoder::new(Limits::default());
    let mut out = Vec::new();
    for chunk in input.chunks(step.max(1)) {
        d.feed(chunk);
        while let Some(r) = d.next() {
            out.push(r.expect("valid"));
        }
    }
    out
}

const SESSION: &[u8] = b"* PREAUTH [CAPABILITY IMAP4rev1 LITERAL+ ENABLE IDLE CONDSTORE QRESYNC MOVE UIDPLUS SPECIAL-USE OBJECTID PREVIEW] courier: logged in\r\n\
* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
* LIST (\\HasChildren) \".\" \"Entw&APw-rfe\"\r\n\
* LIST (\\Noselect) \"/\" {7}\r\nfoo bar\r\n\
* STATUS \"INBOX\" (MESSAGES 231 UIDNEXT 44292 UNSEEN 3 HIGHESTMODSEQ 90060115205545359)\r\n\
A1 OK List completed\r\n\
* 172 EXISTS\r\n\
* OK [UIDVALIDITY 3857529045] UIDs valid\r\n\
* OK [UIDNEXT 4392] Predicted next UID\r\n\
* OK [HIGHESTMODSEQ 715194045007] Highest\r\n\
* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft $Junk)\r\n\
* OK [PERMANENTFLAGS (\\Seen \\Flagged \\*)] Limited\r\n\
* VANISHED (EARLIER) 300:310,405,411\r\n\
A2 OK [READ-WRITE] SELECT completed\r\n\
* 12 FETCH (UID 4827 FLAGS (\\Seen $Forwarded) RFC822.SIZE 4286 MODSEQ (12121231000) EMAILID (M6d99ac3275bb4e) PREVIEW \"Hi there\" BODY[HEADER.FIELDS (DATE FROM)] {30}\r\nFrom: a@b.example\r\nDate: x\r\n\r\n)\r\n\
* 13 FETCH (UID 4828 BODY[] {22}\r\nSubject: x\r\n\r\n) evil\r\n X-GM-MSGID 1278455344230334865 X-GM-LABELS (\\Inbox \"&AMk-quipe\" Work))\r\n\
* 14 FETCH (UID 4829 BODYSTRUCTURE ((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL)(\"application\" \"pdf\" (\"name\" \"r.pdf\") NIL NIL \"base64\" 4096 NIL (\"attachment\" (\"filename\" \"r.pdf\")) NIL NIL) \"mixed\" (\"boundary\" \"b\") NIL NIL NIL) BODY[1]<0> NIL)\r\n\
* SEARCH 2 84 882 (MODSEQ 917162500)\r\n\
A3 NO [THROTTLED] slow down\r\n\
+ idling\r\n\
* 3 EXPUNGE\r\n\
* BYE server shutting down\r\n";

#[test]
fn session_whole_and_byte_by_byte() {
    for step in [SESSION.len(), 1, 7, 64] {
        let r = all(SESSION, step);
        assert_eq!(r.len(), 22, "step {step}");
        match &r[0] {
            Response::State { status: Status::PreAuth, code: Some(Code::Capability(c)), .. } => assert!(c.contains(&"QRESYNC".to_string())),
            x => panic!("{x:?}"),
        }
        assert_eq!(r[1], Response::List { flags: vec!["\\HasNoChildren".into(), "\\Sent".into()], delimiter: Some('/'), name: "Sent".into() });
        assert!(matches!(&r[2], Response::List { name, .. } if name == "Entwürfe"));
        assert!(matches!(&r[3], Response::List { name, .. } if name == "foo bar"));
        assert!(matches!(&r[4], Response::Status { mailbox, items } if mailbox == "INBOX" && items.contains(&("HIGHESTMODSEQ".into(), 90060115205545359))));
        assert!(matches!(&r[5], Response::Tagged { tag, status: Status::Ok, .. } if tag == "A1"));
        assert_eq!(r[6], Response::Exists(172));
        assert!(matches!(&r[7], Response::State { code: Some(Code::UidValidity(3857529045)), .. }));
        assert!(matches!(&r[9], Response::State { code: Some(Code::HighestModSeq(715194045007)), .. }));
        assert!(matches!(&r[12], Response::Vanished { earlier: true, uids } if uids == "300:310,405,411"));
        assert!(matches!(&r[13], Response::Tagged { code: Some(Code::ReadWrite), .. }));
        let Response::Fetch { seq: 12, items } = &r[14] else { panic!() };
        assert!(items.contains(&FetchItem::Uid(4827)));
        assert!(items.contains(&FetchItem::ModSeq(12121231000)));
        assert!(items.contains(&FetchItem::EmailId("M6d99ac3275bb4e".into())));
        assert!(items.contains(&FetchItem::Preview(Some("Hi there".into()))));
        assert!(items.iter().any(|i| matches!(i, FetchItem::Body { section, data: Some(d), .. } if section == "HEADER.FIELDS (DATE FROM)" && d.starts_with(b"From:"))));
        // a literal holding ") evil" can't end the response early
        let Response::Fetch { seq: 13, items } = &r[15] else { panic!("{:?}", r[15]) };
        assert!(items.iter().any(|i| matches!(i, FetchItem::Body { section, data: Some(d), .. } if section.is_empty() && d.ends_with(b") evil\r\n"))));
        assert!(items.contains(&FetchItem::GmMsgId(1278455344230334865)));
        assert!(items.contains(&FetchItem::GmLabels(vec!["\\Inbox".into(), "Équipe".into(), "Work".into()])));
        let Response::Fetch { items, .. } = &r[16] else { panic!() };
        assert!(matches!(&items[1], FetchItem::BodyStructure(Value::List(l)) if l.len() == 7));
        assert!(items.contains(&FetchItem::Body { section: "1".into(), origin: Some(0), data: None }));
        assert_eq!(r[17], Response::Search { ids: vec![2, 84, 882], modseq: Some(917162500) });
        assert!(matches!(&r[18], Response::Tagged { status: Status::No, code: Some(Code::Throttled), .. }));
        assert_eq!(r[19], Response::Continue("idling".into()));
        assert_eq!(r[20], Response::Expunge(3));
        assert!(matches!(&r[21], Response::State { status: Status::Bye, .. }));
    }
}

#[test]
fn literal_contents_are_data() {
    let r = all(b"* 1 FETCH (UID 9 BODY[] {12}\r\nA1 OK done\r\n)\r\nA1 OK real\r\n", 3);
    assert_eq!(r.len(), 2);
    let Response::Fetch { items, .. } = &r[0] else { panic!() };
    assert_eq!(items[1], FetchItem::Body { section: String::new(), origin: None, data: Some(b"A1 OK done\r\n".to_vec()) });
    assert!(matches!(&r[1], Response::Tagged { text, .. } if text == "real"));
}

#[test]
fn limits() {
    let mut d = Decoder::new(Limits { max_literal: 100, ..Limits::default() });
    d.feed(b"* 1 FETCH (BODY[] {101}\r\n");
    assert_eq!(d.next(), Some(Err(Error::TooLarge)));
    let mut d = Decoder::new(Limits { max_depth: 3, ..Limits::default() });
    d.feed(b"* 1 FETCH (X ((((((1)))))))\r\n");
    assert_eq!(d.next(), Some(Err(Error::TooDeep)));
    let mut d = Decoder::new(Limits { max_response: 64, ..Limits::default() });
    d.feed(&[b'x'; 65]);
    assert_eq!(d.next(), Some(Err(Error::TooLarge)));
    let mut d = Decoder::new(Limits::default());
    d.feed(b"garbage\r\n");
    assert!(matches!(d.next(), Some(Err(Error::Syntax(_)))));
}

fn bytes(c: &Cmd) -> Vec<u8> {
    c.chunks.iter().flat_map(|ch| match ch {
        Chunk::Send(b) => b.clone(),
        Chunk::AwaitContinue => b"<WAIT>".to_vec(),
    }).collect()
}

#[test]
fn commands() {
    let mut e = Encoder::new();
    assert_eq!(bytes(&e.enable(&["QRESYNC", "UTF8=ACCEPT", "BAD\r\nX"])), b"A1 ENABLE QRESYNC UTF8=ACCEPT\r\n");
    let known = Set::of(&[1, 2, 3, 7, 9, 10]);
    assert_eq!(bytes(&e.select("Entwürfe", false, Some((67890007, 90060115194045000, Some(&known))), true)), "A2 SELECT Entw&APw-rfe (QRESYNC (67890007 90060115194045000 1:3,7,9:10))\r\n".as_bytes());
    let c = e.uid_fetch(&Set::from(4000), &[FetchWant::Uid, FetchWant::Flags, FetchWant::Envelope, FetchWant::BodyPeek { section: "1.2".into(), partial: Some((0, 2048)) }, FetchWant::BodyPeek { section: "1 OR 1)".into(), partial: None }], Some(77), true);
    assert_eq!(bytes(&c), b"A3 UID FETCH 4000:* (UID FLAGS ENVELOPE BODY.PEEK[1.2]<0.2048> BODY.PEEK[]) (CHANGEDSINCE 77 VANISHED)\r\n");
    assert_eq!(bytes(&e.uid_store(&Set::one(5), StoreOp::Add, &["\\Seen", "$Snoozed", "bad flag", "x)(y"], Some(9))), b"A4 UID STORE 5 (UNCHANGEDSINCE 9) +FLAGS.SILENT (\\Seen $Snoozed)\r\n");
    assert_eq!(bytes(&e.uid_move(&Set::range(1, 4), "Archive/2026 \"Q4\"")), b"A5 UID MOVE 1:4 \"Archive/2026 \\\"Q4\\\"\"\r\n");
    assert_eq!(bytes(&e.append("Sent", &["\\Seen"], b"Subject: hi\r\n\r\nx")), b"A6 APPEND Sent (\\Seen) {16}\r\n<WAIT>Subject: hi\r\n\r\nx\r\n");
    e.set_capabilities(&["LITERAL+".to_string()]);
    assert_eq!(bytes(&e.append("Sent", &[], b"x")), b"A7 APPEND Sent () {1+}\r\nx\r\n");
}

#[test]
fn search_text_never_becomes_syntax() {
    let mut e = Encoder::new();
    e.literal_plus = true;
    let q = Search::And(vec![Search::Since("1-Feb-2026".into()), Search::Text("ALL\r\nA9 LOGOUT".into()), Search::Not(Box::new(Search::From("Zoë".into())))]);
    assert_eq!(
        bytes(&e.uid_search(&q)),
        "A1 UID SEARCH CHARSET UTF-8 (SINCE 1-Feb-2026 TEXT {14+}\r\nALL\r\nA9 LOGOUT NOT FROM {4+}\r\nZoë)\r\n".as_bytes()
    );
}
