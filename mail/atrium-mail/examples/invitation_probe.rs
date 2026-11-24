//! Disposable guest fixture. Seed two synthetic deliveries, or deliver the
//! reviewed outbox operation to a local SMTP peer and inspect its MIME part.
use mail_engine::{Engine, Parser};
use mail_store::{Incoming, Role, Store};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};
const REQUEST: &str = include_str!("../../../atrium-calendar/tests/fixtures/invitation.ics");
const WEEKLY: &str = include_str!("../../../atrium-calendar/tests/fixtures/weekly.ics");
const MOVED: &str = include_str!("../../../atrium-calendar/tests/fixtures/move-one.ics");
const SERIES_MOVE: &str = include_str!("../../../atrium-calendar/tests/fixtures/move-series.ics");
const CANCEL_ONE: &str = include_str!("../../../atrium-calendar/tests/fixtures/cancel-one.ics");
const CANCEL_ALL: &str = include_str!("../../../atrium-calendar/tests/fixtures/cancel-series.ics");
const ORIGINAL: &str = "RECURRENCE-ID:20261208T140000Z";

const BOUNDED: &str = include_str!("../../../atrium-calendar/tests/fixtures/bounded-moves.ics");
const CANCEL_DATES: &str = include_str!("../../../atrium-calendar/tests/fixtures/cancel-dates.ics");
const UNTIL: &str = include_str!("../../../atrium-calendar/tests/fixtures/weekly-until.ics");

fn seed(e: &Engine, mode: &str) {
    let series = mode == "seed-series";
    let grouped = mode == "seed-cancel-dates";
    let bounded = mode == "seed-bounded" || grouped;
    assert!(e.store.accounts().unwrap().is_empty());
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let folder = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let ordinary_move = REQUEST
        .replace("SEQUENCE:1", "SEQUENCE:2")
        .replace("T140000Z", "T150000Z")
        .replace("T144500Z", "T154500Z")
        .replace("Studio A", "Studio B");
    let messages: &[(&str, &str)] = if bounded {
        &[
            ("Four studio meetings", BOUNDED),
            if grouped {
                ("Cancel two studio dates", CANCEL_DATES)
            } else {
                ("Extend to December 29", UNTIL)
            },
        ]
    } else if series {
        &[
            ("Weekly studio", WEEKLY),
            ("Move Wednesday", MOVED),
            ("Weekly time change", SERIES_MOVE),
            ("Cancel Wednesday", CANCEL_ONE),
            ("End weekly studio", CANCEL_ALL),
            ("Unverified cancellation", CANCEL_ALL),
        ]
    } else {
        &[
            ("Studio invitation", REQUEST),
            ("Studio moved", &ordinary_move),
        ]
    };
    for (n, &(title, body)) in messages.iter().enumerate() {
        let id = n as u32 + 1;
        let method = if body.contains("METHOD:CANCEL") {
            "CANCEL"
        } else {
            "REQUEST"
        };
        let sender = if id == 6 {
            "intruder@fixture.invalid"
        } else {
            "mina@fixture.invalid"
        };
        let raw=format!("From: Mina <{sender}>\r\nTo: dev@fixture.invalid\r\nSubject: {title}\r\nMessage-ID: <studio-{id}@fixture.invalid>\r\nDate: Tue, 24 Nov 2026 09:0{id}:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; method={method}\r\n\r\n{}",body.replace('\n',"\r\n")).into_bytes();
        let mut summary = e
            .parser
            .summarize(&[(1, &raw)], false, &[])
            .unwrap()
            .pop()
            .unwrap()
            .unwrap();
        assert_eq!(summary.invite.as_ref().unwrap().sequence, id.min(5));
        // Synthetic sender evidence for the reschedule case. No DNS or live account.
        summary.sender_check = mail_schema::SenderCheck::Verified {
            domain: "fixture.invalid".into(),
            by_provider: false,
        };
        let blob = e.store.put_blob(&raw).unwrap();
        let msg = e
            .store
            .write(|w| {
                w.put(
                    folder,
                    account,
                    id,
                    Incoming {
                        server_id: None,
                        size: raw.len() as u64,
                        internal_date: None,
                        flags: 0,
                        blob,
                        full: true,
                        summary,
                    },
                )
            })
            .unwrap();
        if series || bounded {
            use portcullis_ipc::calendar::{Answer, Reply};
            let scope = match (bounded, id) {
                (true, 1) => Some(format!(
                    "weekly;COUNT=4|{ORIGINAL}|RECURRENCE-ID:20261215T140000Z"
                )),
                (true, 2) if grouped => Some(format!(
                    "cancel-dates|2/{ORIGINAL}|4/RECURRENCE-ID:20261222T140000Z"
                )),
                (true, 2) => Some("weekly;UNTIL=20261229T140000Z".into()),
                (false, 1 | 3) => Some("weekly".into()),
                (false, 2 | 4) => Some(ORIGINAL.into()),
                _ => None,
            };
            if grouped {
                assert_eq!(
                    e.selected_invitation(msg)
                        .unwrap()
                        .source
                        .delivery
                        .unwrap()
                        .scope,
                    scope
                );
            }
            let reply = Reply {
                scope,
                answer: Answer::Maybe,
                attendee: "dev@fixture.invalid".into(),
                organizer: "mina@fixture.invalid".into(),
                uid: "studio-planning@fixture.invalid".into(),
                sequence: id.min(5),
            };
            if method == "CANCEL" {
                assert!(e.send_invitation_reply(msg, blob, &reply).is_err());
            } else {
                let widened = Reply {
                    scope: None,
                    ..reply.clone()
                };
                assert!(e.send_invitation_reply(msg, blob, &widened).is_err());
                if bounded {
                    for scope in ["weekly;COUNT=3", "weekly;COUNT=4", ORIGINAL] {
                        let substituted = Reply {
                            scope: Some(scope.into()),
                            ..reply.clone()
                        };
                        assert!(e.send_invitation_reply(msg, blob, &substituted).is_err());
                    }
                }
                let operation = e.send_invitation_reply(msg, blob, &reply).unwrap();
                let draft = e.undo_send(operation).unwrap().unwrap();
                let ics = std::str::from_utf8(&draft.attachments[0].bytes).unwrap();
                assert_eq!(
                    ics.lines().find(|line| line.starts_with("RECURRENCE-ID")),
                    (!bounded && id == 2).then_some(ORIGINAL)
                );
                assert!(ics.contains("PARTSTAT=TENTATIVE"));
            }
        }
        assert_eq!(
            e.selected_invitation(msg).unwrap().bytes,
            body.replace('\n', "\r\n").as_bytes()
        );
    }
    println!("PASS synthetic Mail deliveries, exact calendar part selected, empty outbox");
    assert!(e.store.outbox().unwrap().is_empty());
}
fn submit(e: &Engine, series: bool) {
    let pending = e.store.outbox().unwrap();
    assert_eq!(pending.len(), 1, "one reviewed Send");
    let (id, o) = &pending[0];
    assert_eq!(o.rcpts, ["mina@fixture.invalid"]);
    let (mail, peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let server = std::thread::spawn(move || {
        let mut peer = BufReader::new(peer);
        peer.get_mut()
            .write_all(b"250 fixture.invalid\r\n")
            .unwrap();
        let mut recipients = Vec::new();
        let mut data = false;
        let mut saved = Vec::new();
        loop {
            let mut line = String::new();
            assert_ne!(peer.read_line(&mut line).unwrap(), 0);
            if data && line != ".\r\n" {
                saved.extend_from_slice(line.as_bytes());
                continue;
            }
            if let Some(to) = line.strip_prefix("RCPT TO:") {
                recipients.push(to.trim().to_string());
            }
            let answer = if line == ".\r\n" {
                data = false;
                "250 queued\r\n"
            } else if line == "DATA\r\n" {
                data = true;
                "354 continue\r\n"
            } else if line == "QUIT\r\n" {
                break;
            } else {
                "250 OK\r\n"
            };
            peer.get_mut().write_all(answer.as_bytes()).unwrap();
        }
        assert_eq!(recipients, ["<mina@fixture.invalid>"]);
        saved
    });
    e.send_now(*id).unwrap();
    assert!(matches!(
        e.deliver(*id, || Ok(mail), true).unwrap(),
        mail_engine::send::Delivery::Sent { .. }
    ));
    let received = server.join().unwrap();
    assert!(String::from_utf8_lossy(&received).contains("text/calendar; method=REPLY"));
    let summary = e
        .parser
        .summarize(&[(1, &received)], false, &[])
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    let invite = summary.invite.unwrap();
    assert_eq!(invite.method, mail_schema::InviteMethod::Reply);
    assert_eq!(invite.sequence, 2);
    assert_eq!(invite.uid, "studio-planning@fixture.invalid");
    assert_eq!(invite.attendees, 1);
    assert_eq!(summary.from[0].addr, "dev@fixture.invalid");
    assert_eq!(summary.to[0].addr, "mina@fixture.invalid");
    let part = summary
        .parts
        .iter()
        .find(|part| part.kind == mail_schema::PartKind::Calendar)
        .unwrap();
    let part = e
        .parser
        .attachment(
            &received,
            &mail_schema::Which::Part(part.path.clone()),
            None,
        )
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&part.bytes)
        .contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:dev@fixture.invalid"));
    let ics = std::str::from_utf8(&part.bytes).unwrap();
    let original_lines: Vec<_> = ics
        .lines()
        .filter(|line| line.starts_with("RECURRENCE-ID"))
        .collect();
    if series {
        assert_eq!(original_lines, [ORIGINAL]);
    } else {
        assert!(original_lines.is_empty());
    }
    assert!(e.store.outbox().unwrap().is_empty());
    let folder = e.store.folder_id(o.account, "INBOX", Role::Inbox).unwrap();
    let msg = e
        .store
        .list(folder, 0, 10)
        .unwrap()
        .into_iter()
        .find(|x| {
            e.store
                .message(x.msg)
                .unwrap()
                .summary
                .invite
                .unwrap()
                .sequence
                == 2
        })
        .unwrap()
        .msg;
    let r = portcullis_ipc::calendar::Reply {
        scope: series.then(|| ORIGINAL.to_owned()),
        answer: portcullis_ipc::calendar::Answer::Accept,
        uid: invite.uid,
        attendee: "dev@fixture.invalid".into(),
        organizer: invite.organizer,
        sequence: 2,
    };
    assert!(e
        .send_invitation_reply(msg, e.store.message(msg).unwrap().full.unwrap(), &r)
        .is_err());
    assert!(e.store.outbox().unwrap().is_empty());
    println!("PASS SMTP received one ACCEPTED for revision 2, organizer only. Replay queued zero additional messages.");
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let e = Engine::new(
        Store::open(Path::new(&args[2])).unwrap(),
        Parser {
            program: args[3].clone().into(),
        },
    );
    match args[1].as_str() {
        mode @ ("seed" | "seed-series" | "seed-bounded" | "seed-cancel-dates") => seed(&e, mode),
        "empty" => assert!(e.store.outbox().unwrap().is_empty()),
        "submit" => submit(&e, false),
        "submit-series" => submit(&e, true),
        "exercise" => {
            seed(&e, "seed");
            let (account, _) = e.store.accounts().unwrap().remove(0);
            let folder = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
            let msg = e
                .store
                .list(folder, 0, 10)
                .unwrap()
                .into_iter()
                .find(|x| {
                    e.store
                        .message(x.msg)
                        .unwrap()
                        .summary
                        .invite
                        .unwrap()
                        .sequence
                        == 2
                })
                .unwrap()
                .msg;
            let blob = e.selected_invitation(msg).unwrap().blob;
            let r = portcullis_ipc::calendar::Reply {
                scope: None,
                answer: portcullis_ipc::calendar::Answer::Accept,
                uid: "studio-planning@fixture.invalid".into(),
                attendee: "dev@fixture.invalid".into(),
                organizer: "mina@fixture.invalid".into(),
                sequence: 2,
            };
            assert!(e.send_invitation_reply(msg, [0; 32], &r).is_err());
            assert!(e.store.outbox().unwrap().is_empty());
            use portcullis_ipc::calendar::Answer;
            for changed in [
                portcullis_ipc::calendar::Reply {
                    organizer: "other@fixture.invalid".into(),
                    ..r.clone()
                },
                portcullis_ipc::calendar::Reply {
                    attendee: "guest@fixture.invalid".into(),
                    ..r.clone()
                },
                portcullis_ipc::calendar::Reply {
                    sequence: 1,
                    ..r.clone()
                },
            ] {
                assert!(e.send_invitation_reply(msg, blob, &changed).is_err());
            }
            for answer in [Answer::Maybe, Answer::Decline] {
                let chosen = portcullis_ipc::calendar::Reply {
                    answer,
                    ..r.clone()
                };
                let queued = e.send_invitation_reply(msg, blob, &chosen).unwrap();
                let draft = e.undo_send(queued).unwrap().unwrap();
                let ics = std::str::from_utf8(&draft.attachments[0].bytes).unwrap();
                assert!(ics.contains(&format!(
                    "ATTENDEE;PARTSTAT={}:mailto:dev@fixture.invalid",
                    answer.partstat()
                )));
                assert!(e.store.outbox().unwrap().is_empty());
            }
            e.send_invitation_reply(msg, blob, &r).unwrap();
            submit(&e, false)
        }
        _ => panic!("seed, empty, exercise or submit expected"),
    }
}
