//! Disposable guest fixture. Seed two synthetic deliveries, or deliver the
//! reviewed outbox operation to a local SMTP peer and inspect its MIME part.
#[allow(dead_code)]
#[path = "../src/bin/atrium-mail/invitation.rs"]
mod invitation;

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
const CANCEL_FROM: &str = include_str!("../../../atrium-calendar/tests/fixtures/cancel-future.ics");
const AFTER_CUTOFF: &str = "RECURRENCE-ID:20261222T140000Z";
const ORIGINAL: &str = "RECURRENCE-ID:20261208T140000Z";

const NEW_YORK_ORIGINAL: &str = "RECURRENCE-ID;TZID=America/New_York:20261208T090000";
const SIBLING: &str = "RECURRENCE-ID:20261215T140000Z";

fn later_occurrence() -> String {
    MOVED
        .replace(ORIGINAL, NEW_YORK_ORIGINAL)
        .replace("SEQUENCE:2", "SEQUENCE:3")
        .replace("20261209T15", "20261210T17")
        .replace("Studio B", "Studio C")
}

const BOUNDED: &str = include_str!("../../../atrium-calendar/tests/fixtures/bounded-moves.ics");
const FUTURE: &str = include_str!("../../../atrium-calendar/tests/fixtures/future.ics");
const FUTURE_SCOPE: &str = "RECURRENCE-ID;RANGE=THISANDFUTURE:20261208T140000Z";
const EXDATES: &str = include_str!("../../../atrium-calendar/tests/fixtures/weekly-exdates.ics");
const CANCEL_DATES: &str = include_str!("../../../atrium-calendar/tests/fixtures/cancel-dates.ics");
const UNTIL: &str = include_str!("../../../atrium-calendar/tests/fixtures/weekly-until.ics");

fn seed(e: &Engine, mode: &str) {
    let series = mode == "seed-series";
    let skip_response = mode == "seed-skip-response";
    let bounded_master = WEEKLY.replace("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=4");
    let skip_update = EXDATES.replace("SEQUENCE:2", "SEQUENCE:4");
    let group_response = mode == "seed-group-response";
    let group_target = MOVED
        .replace(ORIGINAL, AFTER_CUTOFF)
        .replace("SEQUENCE:2", "SEQUENCE:3");
    let range_response = mode == "seed-range-response";
    let earlier = MOVED.replace("20261209T15", "20261218T15");
    let after = MOVED.replace(ORIGINAL, AFTER_CUTOFF);
    let cancellation = CANCEL_FROM.replace("SEQUENCE:2", "SEQUENCE:3");
    let occurrences = mode == "seed-occurrences";
    let later = later_occurrence();
    let sibling = MOVED.replace(ORIGINAL, SIBLING);
    let grouped = mode == "seed-cancel-dates";
    let skipped = mode == "seed-exdates";
    let long = mode == "seed-long-range";
    let reissue = mode == "seed-reissue";
    let one = mode == "seed-restore-one" || reissue;
    let restart = mode == "seed-restart" || one;
    let reinstate = mode == "seed-reinstate" || restart;
    let cancel_future = mode == "seed-cancel-future" || reinstate;
    let future = mode == "seed-future" || long;
    let bounded = mode == "seed-bounded" || grouped || skipped || future || cancel_future;
    assert!(e.store.accounts().unwrap().is_empty());
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let folder = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let ordinary_move = REQUEST
        .replace("SEQUENCE:1", "SEQUENCE:2")
        .replace("T140000Z", "T150000Z")
        .replace("T144500Z", "T154500Z")
        .replace("Studio A", "Studio B");
    let twelve = WEEKLY.replace("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=12");
    let extension = WEEKLY.replace("SEQUENCE:1", "SEQUENCE:3");
    let spring = FUTURE
        .replace("SEQUENCE:2", "SEQUENCE:4")
        .replace("20261208T140000Z", "20270309T140000Z")
        .replace("20261209T160000Z", "20270310T180000Z")
        .replace("20261209T170000Z", "20270310T190000Z");
    let spring_scope = FUTURE_SCOPE.replace("20261208", "20270309");
    let cut = include_str!("../../../atrium-calendar/tests/fixtures/cancel-future.ics")
        .replace("20261215", "20261208");
    let resume = include_str!("../../../atrium-calendar/tests/fixtures/restart-future.ics")
        .replace("20261222", "20261229")
        .replace("20261223", "20261230");
    let restored_series = [
        ("Weekly studio meetings", WEEKLY),
        ("Cancel December 8 onward", &cut),
        ("Restart December 29 onward", &resume),
        (
            "Bring back December 15 only",
            include_str!("../../../atrium-calendar/tests/fixtures/restore-one.ics"),
        ),
        (
            "Cancel the restored meeting",
            include_str!("../../../atrium-calendar/tests/fixtures/cancel-restored.ics"),
        ),
        (
            "Reissue the December 15 meeting",
            include_str!("../../../atrium-calendar/tests/fixtures/reissue-one.ics"),
        ),
    ];
    let messages: &[(&str, &str)] = if reissue {
        &restored_series
    } else if one {
        &restored_series[..4]
    } else if reinstate {
        &[
            ("Weekly studio meetings", WEEKLY),
            (
                "Cancel December 15 onward",
                include_str!("../../../atrium-calendar/tests/fixtures/cancel-future.ics"),
            ),
            if restart {
                (
                    "Restart December 22 onward",
                    include_str!("../../../atrium-calendar/tests/fixtures/restart-future.ics"),
                )
            } else {
                (
                    "Return December 15 onward",
                    include_str!("../../../atrium-calendar/tests/fixtures/reinstate-future.ics"),
                )
            },
        ]
    } else if cancel_future {
        &[
            ("Weekly studio meetings", WEEKLY),
            (
                "Cancel December 15 onward",
                include_str!("../../../atrium-calendar/tests/fixtures/cancel-future.ics"),
            ),
        ]
    } else if long {
        &[
            ("Twelve studio meetings", &twelve),
            ("Move the remaining meetings", FUTURE),
            ("Continue weekly", &extension),
            ("Spring time change", &spring),
        ]
    } else if bounded {
        &[
            ("Four studio meetings", BOUNDED),
            if grouped {
                ("Cancel two studio dates", CANCEL_DATES)
            } else if future {
                ("Change this and future meetings", FUTURE)
            } else if skipped {
                ("Two skipped Tuesdays", EXDATES)
            } else {
                ("Extend to December 29", UNTIL)
            },
        ]
    } else if skip_response {
        &[
            ("Weekly studio", &bounded_master),
            ("December 15 moved to December 9", &sibling),
            ("December 22 moved to December 9", &group_target),
            ("Skip December 8 and 22", &skip_update),
        ]
    } else if group_response {
        &[
            ("Weekly studio", WEEKLY),
            ("December 15 moved to December 9", &sibling),
            ("December 22 moved to December 9", &group_target),
            ("Cancel December 8 and 22", CANCEL_DATES),
        ]
    } else if range_response {
        &[
            ("Weekly studio", WEEKLY),
            ("December 8 moved to December 18", &earlier),
            ("December 22 moved to December 9", &after),
            ("Cancel from December 15", &cancellation),
        ]
    } else if occurrences {
        &[
            ("Weekly studio", WEEKLY),
            ("Move December 8", MOVED),
            ("Move December 8 again", &later),
            ("Move December 15", &sibling),
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
        let sender = if series && id == 6 {
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
        let sequence = if skip_response {
            [1, 2, 3, 4][n]
        } else if group_response {
            [1, 2, 3, 2][n]
        } else if range_response {
            [1, 2, 2, 3][n]
        } else if occurrences && id == 4 {
            2
        } else if series {
            id.min(5)
        } else {
            id
        };
        assert_eq!(summary.invite.as_ref().unwrap().sequence, sequence);
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
                (true, 1) if cancel_future => Some("weekly".into()),
                (true, 2) if one => Some(FUTURE_SCOPE.into()),
                (true, 3) if one => Some(FUTURE_SCOPE.replace("20261208", "20261229")),
                (true, 4..=6) if one => Some("RECURRENCE-ID:20261215T140000Z".into()),
                (true, 3) if restart => Some(FUTURE_SCOPE.replace("20261208", "20261222")),
                (true, 2 | 3) if cancel_future => {
                    Some(FUTURE_SCOPE.replace("20261208", "20261215"))
                }
                (true, 1) if long => Some("weekly;COUNT=12".into()),
                (true, 3) if long => Some("weekly".into()),
                (true, 4) if long => Some(spring_scope.clone()),
                (true, 1) => Some(format!(
                    "weekly;COUNT=4|{ORIGINAL}|RECURRENCE-ID:20261215T140000Z"
                )),
                (true, 2) if future => Some(FUTURE_SCOPE.into()),
                (true, 2) if skipped => {
                    Some("weekly;COUNT=4|EXDATE:20261208T140000Z|EXDATE:20261222T140000Z".into())
                }
                (true, 2) if grouped => Some(format!(
                    "cancel-dates|2/{ORIGINAL}|4/RECURRENCE-ID:20261222T140000Z"
                )),
                (true, 2) => Some("weekly;UNTIL=20261229T140000Z".into()),
                (false, 1 | 3) => Some("weekly".into()),
                (false, 2 | 4) => Some(ORIGINAL.into()),
                _ => None,
            };
            if grouped || skipped || future || cancel_future {
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
                sequence,
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
                    if (reinstate && id == 3) || (one && id >= 4) {
                        reply.scope.as_deref()
                    } else if long && id == 4 {
                        Some(spring_scope.as_str())
                    } else if future && id == 2 {
                        Some(FUTURE_SCOPE)
                    } else {
                        (!bounded && id == 2).then_some(ORIGINAL)
                    }
                );
                if reissue && id == 6 {
                    assert!(ics.lines().any(|line| line == "SEQUENCE:6"));
                    assert!(!ics.contains("RANGE="));
                }
                assert!(ics.contains("PARTSTAT=TENTATIVE"));
                if skipped && id == 2 {
                    assert!(draft.text.contains("2 skipped dates"));
                    assert!(!ics.contains("EXDATE"));
                }
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
fn submit(e: &Engine, sequence: u32, scope: Option<&str>) {
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
    if scope == Some(NEW_YORK_ORIGINAL) {
        let text = summary.text.as_deref().unwrap();
        assert!(text.contains("17:00") && !text.contains("2026-12-11") && !text.contains("11:30"));
    }
    let invite = summary.invite.unwrap();
    assert_eq!(invite.method, mail_schema::InviteMethod::Reply);
    assert_eq!(invite.sequence, sequence);
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
    assert_eq!(original_lines, scope.into_iter().collect::<Vec<_>>());
    assert!(e.store.outbox().unwrap().is_empty());
    let folder = e.store.folder_id(o.account, "INBOX", Role::Inbox).unwrap();
    let msg = e
        .store
        .list(folder, 0, 10)
        .unwrap()
        .into_iter()
        .find(|x| {
            let m = e.store.message(x.msg).unwrap();
            m.summary.invite.is_some_and(|i| i.sequence == sequence)
                && e.selected_invitation(x.msg)
                    .unwrap()
                    .source
                    .delivery
                    .and_then(|d| d.scope)
                    .as_deref()
                    == scope
        })
        .unwrap()
        .msg;
    let r = portcullis_ipc::calendar::Reply {
        scope: scope.map(str::to_owned),
        answer: portcullis_ipc::calendar::Answer::Accept,
        uid: invite.uid,
        attendee: "dev@fixture.invalid".into(),
        organizer: invite.organizer,
        sequence,
    };
    assert!(e
        .send_invitation_reply(msg, e.store.message(msg).unwrap().full.unwrap(), &r)
        .is_err());
    assert!(e.store.outbox().unwrap().is_empty());
    println!("PASS SMTP received one ACCEPTED for revision {sequence}, organizer only. Replay queued zero additional messages.");
}
// The guest fixture supplies sender evidence explicitly and never contacts a provider.
fn background_batch(e: &Engine) {
    let account = e.store.accounts().unwrap()[0].0;
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    for (uid, seq, hour, verified) in [
        (3, 5, 17, true),
        (4, 4, 16, true),
        (5, 5, 17, true),
        (6, 8, 18, false),
    ] {
        let body = REQUEST
            .replace("SEQUENCE:1", &format!("SEQUENCE:{seq}"))
            .replace("T14", &format!("T{hour}"))
            .replace("Studio A", "Studio C");
        put_revision(e, account, inbox, uid, &body, verified);
    }
}

fn put_revision(
    e: &Engine,
    account: u32,
    inbox: u32,
    uid: u32,
    body: &str,
    verified: bool,
) -> mail_store::MsgId {
    let method = if body.contains("METHOD:CANCEL") {
        "CANCEL"
    } else {
        "REQUEST"
    };
    let raw = format!("From: Mina <mina@fixture.invalid>\r\nTo: dev@fixture.invalid\r\nSubject: Studio update {uid}\r\nMessage-ID: <update-{uid}@fixture.invalid>\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; method={method}\r\n\r\n{}",body.replace('\n',"\r\n")).into_bytes();
    let mut summary = e
        .parser
        .summarize(&[(1, &raw)], false, &[])
        .unwrap()
        .pop()
        .unwrap()
        .unwrap();
    if verified {
        summary.sender_check = mail_schema::SenderCheck::Verified {
            domain: "fixture.invalid".into(),
            by_provider: false,
        };
    }
    let blob = e.store.put_blob(&raw).unwrap();
    e.store
        .write(|w| {
            w.put(
                inbox,
                account,
                uid,
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
        .unwrap()
}

fn check_background(e: &Engine) {
    let account = e.store.accounts().unwrap()[0].0;
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    for uid in 2..=6 {
        let (_, flags) = e.store.at(inbox, uid).unwrap().unwrap();
        assert_eq!(
            flags & mail_store::flag::SEEN,
            0,
            "fixture UID {uid} was read"
        );
    }
    assert!(e.store.pending_calendar(0).unwrap().is_empty());
    assert!(e.store.outbox().unwrap().is_empty());
}

// A real Mail confirmation on the disposable guest. The driver waits for the
// original time on screen before allowing a new delivery into this fixture.
fn during_confirmation(e: &Engine, dir: &Path) {
    use mail_engine::{invitations::LatestInvitation, Error};
    use portcullis_ipc::calendar::{Answer, Reply};
    seed(e, "seed");
    let account = e.store.accounts().unwrap()[0].0;
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let msg = e.store.at(inbox, 2).unwrap().unwrap().0;
    let blob = e.selected_invitation(msg).unwrap().blob;
    let reply = Reply {
        scope: None,
        answer: Answer::Maybe,
        uid: "studio-planning@fixture.invalid".into(),
        attendee: "dev@fixture.invalid".into(),
        organizer: "mina@fixture.invalid".into(),
        sequence: 2,
    };
    let latest = check_open_window(e, dir, msg, blob, &reply, || {
        background_batch(e);
        e.store.at(inbox, 5).unwrap().unwrap().0
    });
    let latest_reply = Reply {
        sequence: 5,
        ..reply
    };
    let blob = e.selected_invitation(latest).unwrap().blob;
    // Neither a different attendee nor another account sharing the UID
    // supersedes this choice. A byte-for-byte replay also permits sending.
    let higher = REQUEST.replace("SEQUENCE:1", "SEQUENCE:7");
    put_revision(
        e,
        account,
        inbox,
        7,
        &higher.replace("dev@fixture.invalid", "other@fixture.invalid"),
        true,
    );
    let second = e.store.add_account("dev@fixture.invalid").unwrap();
    let other = e.store.folder_id(second, "INBOX", Role::Inbox).unwrap();
    put_revision(e, second, other, 1, &higher, true);
    let op = e
        .send_invitation_reply(latest, blob, &latest_reply)
        .unwrap();
    assert!(e.undo_send(op).unwrap().is_some());
    let conflicting = REQUEST.replace("SEQUENCE:1", "SEQUENCE:5");
    put_revision(e, account, inbox, 8, &conflicting, true);
    assert!(matches!(
        e.send_invitation_reply(latest, blob, &latest_reply),
        Err(Error::InvitationChanged(LatestInvitation::Conflict {
            sequence: 5
        }))
    ));
    let cancel = higher.replace("METHOD:REQUEST", "METHOD:CANCEL");
    let cancel_id = put_revision(e, account, inbox, 9, &cancel, true);
    assert!(matches!(e.send_invitation_reply(latest,blob,&latest_reply),
        Err(Error::InvitationChanged(LatestInvitation::Update{msg, sequence:7, when, ..}))
        if msg == cancel_id && when.contains("Cancelled")));
    assert!(e.store.outbox().unwrap().is_empty());
    println!("PASS live confirmation, late verified move, duplicate, identity isolation, conflict and cancellation");
}

fn check_open_window(
    e: &Engine,
    dir: &Path,
    msg: u64,
    blob: mail_store::Hash,
    reply: &portcullis_ipc::calendar::Reply,
    deliver: impl FnOnce() -> u64,
) -> u64 {
    let mut window = invitation::ResponseWindow::new(e, msg, blob, reply.clone()).unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(120);
    let mut deliver = Some(deliver);
    let mut latest = None;
    loop {
        assert!(
            std::time::Instant::now() < until,
            "timed out waiting for fixture input"
        );
        if dir.join("deliver").exists() {
            if let Some(send) = deliver.take() {
                latest = Some(send());
            }
        }
        if let Some(done) = window.tick(e) {
            assert!(matches!(done, invitation::Finished::Review(id, _) if Some(id) == latest));
            assert!(e.store.outbox().unwrap().is_empty());
            return latest.unwrap();
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn during_occurrence(e: &Engine, dir: &Path) {
    use mail_engine::{invitations::LatestInvitation, Error};
    use portcullis_ipc::calendar::{Answer, Reply};
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let put = |id, body: &str, verified| put_revision(e, account, inbox, id, body, verified);
    let first = put(1, MOVED, true);
    let sibling = put(2, &MOVED.replace(ORIGINAL, SIBLING), true);
    let blob = |id| e.store.message(id).unwrap().full.unwrap();
    let reply = Reply {
        scope: Some(ORIGINAL.into()),
        sequence: 2,
        attendee: "dev@fixture.invalid".into(),
        organizer: "mina@fixture.invalid".into(),
        uid: "studio-planning@fixture.invalid".into(),
        answer: Answer::Maybe,
    };
    let later = later_occurrence();
    let latest = check_open_window(e, dir, first, blob(first), &reply, || {
        put(3, &later.replace("SEQUENCE:3", "SEQUENCE:9"), false);
        put(4, &later, true)
    });
    let sibling_reply = Reply {
        scope: Some(SIBLING.into()),
        ..reply.clone()
    };
    let op = e
        .send_invitation_reply(sibling, blob(sibling), &sibling_reply)
        .unwrap();
    let draft = e.undo_send(op).unwrap().unwrap();
    let ics = std::str::from_utf8(&draft.attachments[0].bytes).unwrap();
    assert!(ics.lines().any(|s| s == SIBLING));
    let fresh = Reply {
        scope: Some(NEW_YORK_ORIGINAL.into()),
        sequence: 3,
        ..reply.clone()
    };
    let op = e
        .send_invitation_reply(latest, blob(latest), &fresh)
        .unwrap();
    let alias = put(5, &later.replace(NEW_YORK_ORIGINAL, ORIGINAL), true);
    let utc_reply = Reply {
        sequence: 3,
        ..reply
    };
    assert!(e
        .send_invitation_reply(alias, blob(alias), &utc_reply)
        .unwrap_err()
        .to_string()
        .contains("already queued"));
    assert_eq!(e.store.outbox().unwrap().len(), 1);
    e.undo_send(op).unwrap().unwrap();
    put(6, &later.replace("Studio C", "Studio D"), true);
    assert!(matches!(
        e.send_invitation_reply(latest, blob(latest), &fresh),
        Err(Error::InvitationChanged(LatestInvitation::Conflict {
            sequence: 3
        }))
    ));
    let cancel = put(7, CANCEL_ONE, true);
    assert!(
        matches!(e.send_invitation_reply(latest,blob(latest),&fresh),
        Err(Error::InvitationChanged(LatestInvitation::Update {msg, when, ..})) if msg == cancel && when.contains("Cancelled"))
    );
    assert!(e.store.outbox().unwrap().is_empty());
    println!("PASS live confirmation for a moved occurrence, sibling RSVP, zoned replay receipt, conflict and cancellation");
}

fn during_range_cancellation(e: &Engine, dir: &Path) {
    use mail_engine::{invitations::LatestInvitation, Error};
    use portcullis_ipc::calendar::{Answer, Reply};
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let put = |id, body: &str, verified| put_revision(e, account, inbox, id, body, verified);
    let earlier = put(1, &MOVED.replace("20261209T15", "20261218T15"), true);
    let at_cutoff = put(2, &MOVED.replace(ORIGINAL, SIBLING), true);
    let later = put(3, &MOVED.replace(ORIGINAL, AFTER_CUTOFF), true);
    let blob = |id| e.store.message(id).unwrap().full.unwrap();
    let reply = Reply {
        scope: Some(AFTER_CUTOFF.into()),
        sequence: 2,
        attendee: "dev@fixture.invalid".into(),
        organizer: "mina@fixture.invalid".into(),
        uid: "studio-planning@fixture.invalid".into(),
        answer: Answer::Accept,
    };
    let latest = check_open_window(e, dir, later, blob(later), &reply, || {
        // A higher unverified cancellation would incorrectly include the earlier reply.
        put(
            4,
            &CANCEL_FROM
                .replace("20261215", "20261201")
                .replace("SEQUENCE:2", "SEQUENCE:9"),
            false,
        );
        let equal = put(5, CANCEL_FROM, true);
        assert!(
            matches!(e.send_invitation_reply(later, blob(later), &reply),
            Err(Error::InvitationChanged(LatestInvitation::Update {msg, sequence: 2, ..})) if msg == equal)
        );
        let newer = CANCEL_FROM.replace("SEQUENCE:2", "SEQUENCE:3");
        put(
            6,
            &newer.replace(
                ":20261215T140000Z",
                ";TZID=America/New_York:20261215T090000",
            ),
            true,
        );
        put(7, &newer, true)
    });
    let boundary_reply = Reply {
        scope: Some(SIBLING.into()),
        ..reply.clone()
    };
    assert!(
        matches!(e.send_invitation_reply(at_cutoff, blob(at_cutoff), &boundary_reply),
        Err(Error::InvitationChanged(LatestInvitation::Update { msg, sequence:3, when, .. }))
        if msg == latest && when == "Cancelled from 2026-12-15 14:00 · UTC onward")
    );
    assert!(e.store.outbox().unwrap().is_empty());
    let earlier_reply = Reply {
        scope: Some(ORIGINAL.into()),
        ..reply
    };
    e.send_invitation_reply(earlier, blob(earlier), &earlier_reply)
        .unwrap();
    submit(e, 2, Some(ORIGINAL));
    println!("PASS live confirmation, inclusive cancellation cutoff, later original moved earlier blocked, earlier original moved later delivered exactly once");
}

fn during_group_cancellation(e: &Engine, dir: &Path) {
    use mail_engine::{invitations::LatestInvitation, Error};
    use portcullis_ipc::calendar::{Answer, Reply};
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let put = |id, body: &str, verified| put_revision(e, account, inbox, id, body, verified);
    let request = MOVED.replace("SEQUENCE:2", "SEQUENCE:3");
    let target = put(1, &request.replace(ORIGINAL, AFTER_CUTOFF), true);
    let sibling = put(2, &MOVED.replace(ORIGINAL, SIBLING), true);
    let first = put(3, &request, true);
    let blob = |id| e.store.message(id).unwrap().full.unwrap();
    let reply = Reply {
        scope: Some(AFTER_CUTOFF.into()),
        sequence: 3,
        attendee: "dev@fixture.invalid".into(),
        organizer: "mina@fixture.invalid".into(),
        uid: "studio-planning@fixture.invalid".into(),
        answer: Answer::Accept,
    };
    let latest = check_open_window(e, dir, target, blob(target), &reply, || {
        // A same-revision conflict inspected before the group must yield to
        // the higher revision on the group's second date.
        put(
            4,
            &request
                .replace(ORIGINAL, AFTER_CUTOFF)
                .replace("Studio B", "Studio C"),
            true,
        );
        put(
            5,
            &CANCEL_DATES
                .replace(AFTER_CUTOFF, SIBLING)
                .replace("SEQUENCE:4", "SEQUENCE:9"),
            false,
        );
        put(
            6,
            &CANCEL_DATES
                .replace("SEQUENCE:2", "SEQUENCE:0\nLOCATION:Sibling venue")
                .replace(
                    AFTER_CUTOFF,
                    "RECURRENCE-ID;TZID=America/New_York:20261222T090000",
                ),
            true,
        )
    });
    assert!(
        matches!(e.send_invitation_reply(target, blob(target), &reply),
        Err(Error::InvitationChanged(LatestInvitation::Update {msg, sequence:4, when, location}))
        if msg == latest && location.is_empty() && when == "This meeting is one of 2 cancelled dates")
    );
    // A high revision on the first date cannot cancel a newer reply on the second.
    let stale = CANCEL_DATES
        .replace("SEQUENCE:2", "SEQUENCE:8")
        .replace("SEQUENCE:4", "SEQUENCE:1");
    put(7, &stale, true);
    assert!(
        matches!(e.send_invitation_reply(target, blob(target), &reply),
        Err(Error::InvitationChanged(LatestInvitation::Update { msg, sequence:4, .. })) if msg == latest)
    );
    let first_reply = Reply {
        scope: Some(ORIGINAL.into()),
        ..reply.clone()
    };
    assert!(matches!(
        e.send_invitation_reply(first, blob(first), &first_reply),
        Err(Error::InvitationChanged(LatestInvitation::Update {
            sequence: 8,
            ..
        }))
    ));
    assert!(e.store.outbox().unwrap().is_empty());
    let untouched = Reply {
        sequence: 2,
        scope: Some(SIBLING.into()),
        ..reply
    };
    e.send_invitation_reply(sibling, blob(sibling), &untouched)
        .unwrap();
    submit(e, 2, Some(SIBLING));
    println!("PASS live confirmation, per-date revisions in both orders, unlisted meeting delivered once");
}

fn during_weekly_skip(e: &Engine, dir: &Path) {
    use mail_engine::{invitations::LatestInvitation, Error};
    use portcullis_ipc::calendar::{Answer, Reply};
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let put = |id, body: &str, verified| put_revision(e, account, inbox, id, body, verified);
    let target = put(
        1,
        &MOVED
            .replace(ORIGINAL, AFTER_CUTOFF)
            .replace("SEQUENCE:2", "SEQUENCE:3"),
        true,
    );
    let sibling = put(2, &MOVED.replace(ORIGINAL, SIBLING), true);
    let blob = |id| e.store.message(id).unwrap().full.unwrap();
    let reply = Reply {
        scope: Some(AFTER_CUTOFF.into()),
        sequence: 3,
        attendee: "dev@fixture.invalid".into(),
        organizer: "mina@fixture.invalid".into(),
        uid: "studio-planning@fixture.invalid".into(),
        answer: Answer::Accept,
    };
    let exclusions = EXDATES.replace("SEQUENCE:2", "SEQUENCE:4").replace(
        "EXDATE:20261208T140000Z,20261222T140000Z",
        "EXDATE:20261208T140000Z\nEXDATE;TZID=America/New_York:20261222T090000",
    );
    let skip = check_open_window(e, dir, target, blob(target), &reply, || {
        put(
            3,
            &EXDATES
                .replace("20261222", "20261215")
                .replace("SEQUENCE:2", "SEQUENCE:9"),
            false,
        );
        put(4, &exclusions, true)
    });
    assert!(
        matches!(e.send_invitation_reply(target, blob(target), &reply),
        Err(Error::InvitationChanged(LatestInvitation::Update { msg, sequence:4, when, location }))
        if msg == skip && when.contains("skips this occurrence") && location.is_empty())
    );
    assert!(e.store.outbox().unwrap().is_empty());
    let other = Reply {
        scope: Some(SIBLING.into()),
        sequence: 2,
        ..reply.clone()
    };
    e.send_invitation_reply(sibling, blob(sibling), &other)
        .unwrap();
    submit(e, 2, Some(SIBLING));
    let reinstated = put(5, &WEEKLY.replace("SEQUENCE:1", "SEQUENCE:5"), true);
    assert!(
        matches!(e.send_invitation_reply(target, blob(target), &reply),
        Err(Error::InvitationChanged(LatestInvitation::Update {msg, sequence:5, when, ..}))
        if msg == reinstated && when.contains("revised after the change"))
    );
    put(6, &exclusions.replace("SEQUENCE:4", "SEQUENCE:5"), true);
    assert!(matches!(
        e.send_invitation_reply(target, blob(target), &reply),
        Err(Error::InvitationChanged(LatestInvitation::Conflict {
            sequence: 5
        }))
    ));
    assert!(e.store.outbox().unwrap().is_empty());
    println!("PASS live confirmation, verified weekly exclusion, unlisted occurrence delivered once, newer revision and conflict");
}

/// A weekly invitation bundles a move of December 22. December 15 and 22 were
/// each moved to the same December 9 time, so only the original slot tells them
/// apart: December 22 pauses at Send, December 15 is delivered once.
fn weekly_move(e: &Engine) {
    use mail_engine::{invitations::LatestInvitation, Error};
    use portcullis_ipc::calendar::{Answer, Reply};
    let account = e.store.add_account("dev@fixture.invalid").unwrap();
    let inbox = e.store.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let put = |id, body: &str, verified| put_revision(e, account, inbox, id, body, verified);
    let master = WEEKLY.replace("FREQ=WEEKLY", "FREQ=WEEKLY;COUNT=4");
    put(1, &master, true);
    let sibling = put(2, &MOVED.replace(ORIGINAL, SIBLING), true);
    let target = put(
        3,
        &MOVED
            .replace(ORIGINAL, AFTER_CUTOFF)
            .replace("SEQUENCE:2", "SEQUENCE:3"),
        true,
    );
    let blob = |id| e.store.message(id).unwrap().full.unwrap();
    let reply = Reply {
        scope: Some(AFTER_CUTOFF.into()),
        sequence: 3,
        attendee: "dev@fixture.invalid".into(),
        organizer: "mina@fixture.invalid".into(),
        uid: "studio-planning@fixture.invalid".into(),
        answer: Answer::Accept,
    };
    let other = Reply {
        scope: Some(SIBLING.into()),
        sequence: 2,
        ..reply.clone()
    };
    // The weekly series at sequence 4 with one bundled override: December 22,
    // given in New York time, moved on to December 23 in Studio C.
    let bundle = |seq: &str, to: &str, place: &str| {
        let end = MOVED.find("END:VCALENDAR").unwrap();
        let start = MOVED.find("BEGIN:VEVENT").unwrap();
        let moved = MOVED[start..end]
            .replace(ORIGINAL, "RECURRENCE-ID;TZID=America/New_York:20261222T090000")
            .replace("SEQUENCE:2", seq)
            .replace("20261209T15", to)
            .replace("Studio B", place);
        let weekly = master.replace("SEQUENCE:1", seq);
        let at = weekly.find("END:VCALENDAR").unwrap();
        format!("{}{moved}END:VCALENDAR\n", &weekly[..at])
    };
    // The same move restated by the weekly invitation pauses nothing.
    put(4, &bundle("SEQUENCE:4", "20261209T15", "Studio B"), true);
    e.send_invitation_reply(target, blob(target), &reply).unwrap();
    submit(e, 3, Some(AFTER_CUTOFF));
    // An unverified bundle is not evidence either way.
    put(5, &bundle("SEQUENCE:9", "20261230T10", "Studio D"), false);
    e.send_invitation_reply(target, blob(target), &reply).unwrap();
    submit(e, 3, Some(AFTER_CUTOFF));
    let moved = put(6, &bundle("SEQUENCE:5", "20261223T11", "Studio C"), true);
    let paused = e.send_invitation_reply(target, blob(target), &reply);
    assert!(
        matches!(&paused,
        Err(Error::InvitationChanged(LatestInvitation::Update { msg, sequence: 5, when, location }))
        if *msg == moved && when.contains("moves this occurrence") && when.contains("2026-12-23 · 11:00–11:45 · UTC") && location == "Studio C"),
        "{paused:?}"
    );
    assert!(e.store.outbox().unwrap().is_empty());
    // December 15 has the same proposed time but another original slot.
    e.send_invitation_reply(sibling, blob(sibling), &other).unwrap();
    submit(e, 2, Some(SIBLING));
    // The same sequence moving December 22 somewhere else is a conflict.
    put(7, &bundle("SEQUENCE:5", "20261224T11", "Studio C"), true);
    assert!(matches!(
        e.send_invitation_reply(target, blob(target), &reply),
        Err(Error::InvitationChanged(LatestInvitation::Conflict { sequence: 5 }))
    ));
    assert!(e.store.outbox().unwrap().is_empty());
    println!("PASS weekly bundled move pauses only its original slot, same-time sibling delivered once, restated move, unverified bundle and conflict");
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
        mode @ ("seed"
        | "seed-range-response"
        | "seed-group-response"
        | "seed-skip-response"
        | "seed-occurrences"
        | "seed-series"
        | "seed-bounded"
        | "seed-cancel-dates"
        | "seed-exdates"
        | "seed-future"
        | "seed-long-range"
        | "seed-cancel-future"
        | "seed-reinstate"
        | "seed-restart"
        | "seed-restore-one"
        | "seed-reissue") => seed(&e, mode),
        "during-weekly-skip" => during_weekly_skip(&e, Path::new(&args[2])),
        "weekly-move" => weekly_move(&e),
        "during-group-cancellation" => during_group_cancellation(&e, Path::new(&args[2])),
        "during-range-cancellation" => during_range_cancellation(&e, Path::new(&args[2])),
        "during-occurrence" => during_occurrence(&e, Path::new(&args[2])),
        "submit-occurrence" => submit(&e, 3, Some(NEW_YORK_ORIGINAL)),
        "submit-sibling" => submit(&e, 2, Some(SIBLING)),
        "during-confirmation" => during_confirmation(&e, Path::new(&args[2])),
        "background-batch" => background_batch(&e),
        "background-check" => check_background(&e),
        "empty" => assert!(e.store.outbox().unwrap().is_empty()),
        "submit" => submit(&e, 2, None),
        "submit-series" => submit(&e, 2, Some(ORIGINAL)),
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
            submit(&e, 2, None)
        }
        _ => panic!("seed, empty, exercise or submit expected"),
    }
}
