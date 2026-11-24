use mail_schema::{Address, Summary};
use mail_store::{flag, Incoming, Role, Store};

fn summary(subject: &str, date: i64) -> Summary {
    Summary { subject: subject.into(), date: Some(date), from: vec![Address { name: "Priya".into(), addr: "p@example.org".into(), name_looks_like_address: false }], ..Default::default() }
}

fn inc(s: &Store, subject: &str, date: i64, sid: Option<&str>, full: bool) -> Incoming {
    let blob = s.put_blob(format!("Subject: {subject}\r\n\r\n{}", if full { "body" } else { "" }).as_bytes()).unwrap();
    Incoming { server_id: sid.map(Into::into), size: 100, internal_date: None, flags: 0, blob, full, summary: summary(subject, date) }
}

#[test]
fn list_is_newest_first_and_dedups_by_server_id() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let a = s.add_account("priya@example.org").unwrap();
    let inbox = s.folder_id(a, "INBOX", Role::Inbox).unwrap();
    let all = s.folder_id(a, "[Gmail]/All Mail", Role::All).unwrap();
    assert_eq!(s.folder_id(a, "INBOX", Role::Inbox).unwrap(), inbox);
    let m1 = inc(&s, "old", 1_000, Some("e1"), false);
    let m2 = inc(&s, "new", 2_000, Some("e2"), false);
    let m3 = inc(&s, "middle", 1_500, Some("e3"), false);
    let id1 = s.write(|w| { let i = w.put(inbox, a, 10, m1.clone())?; w.put(inbox, a, 11, m2)?; w.put(inbox, a, 12, m3)?; Ok(i) }).unwrap();
    let rows = s.list(inbox, 0, 10).unwrap();
    assert_eq!(rows.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["new", "middle", "old"]);
    assert_eq!(s.list(inbox, 1, 1).unwrap()[0].subject, "middle");
    // The same message in All Mail is the same message.
    let again = s.write(|w| w.put(all, a, 900, inc(&s, "old", 1_000, Some("e1"), false))).unwrap();
    assert_eq!(again, id1);
    assert_eq!(s.message_count().unwrap(), 3);
    assert_eq!(s.message(id1).unwrap().locations, vec![(inbox, 10), (all, 900)]);
    // Text arrives: the header-only record is upgraded, not duplicated.
    s.write(|w| w.put(inbox, a, 10, inc(&s, "old", 1_000, Some("e1"), true))).unwrap();
    let m = s.message(id1).unwrap();
    assert!(m.full.is_some());
    assert!(s.blob(&m.full.unwrap()).unwrap().ends_with(b"body"));
    assert!(s.list(inbox, 2, 1).unwrap()[0].has_text);
    // Flags, then removal from one folder and then the other.
    s.write(|w| w.set_flags(inbox, 10, flag::SEEN | flag::FLAGGED)).unwrap();
    assert_eq!(s.at(inbox, 10).unwrap(), Some((id1, flag::SEEN | flag::FLAGGED)));
    s.write(|w| w.remove(inbox, 10)).unwrap();
    assert_eq!(s.count(inbox).unwrap(), 2);
    assert_eq!(s.message_count().unwrap(), 3);
    s.write(|w| w.remove(all, 900)).unwrap();
    assert_eq!(s.message_count().unwrap(), 2);
    assert_eq!(s.by_server_id(a, "e1").unwrap(), None);
    // UIDVALIDITY change.
    s.write(|w| w.clear_folder(inbox)).unwrap();
    assert_eq!(s.count(inbox).unwrap(), 0);
    assert_eq!(s.uids(inbox).unwrap(), Vec::<u32>::new());
    assert_eq!(flag::from_imap(&["\\Seen", "\\Answered", "$Junk", "$Phishing", "$Forwarded"]), flag::SEEN | flag::ANSWERED | flag::JUNK | flag::PHISHING);
}

#[test]
fn a_blob_that_changed_on_disk_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let h = s.put_blob(b"hello").unwrap();
    assert_eq!(s.blob(&h).unwrap(), b"hello");
    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(d.path().join("blobs").join(&hex[..2]).join(&hex[2..]), b"evil").unwrap();
    assert!(s.blob(&h).is_err());
}

fn mail(s: &Store, subject: &str, date: i64, mid: &str, refs: &[&str], from: &str) -> Incoming {
    let blob = s.put_blob(format!("Message-ID: <{mid}>\r\nSubject: {subject}\r\n\r\nbody").as_bytes()).unwrap();
    let summary = Summary {
        subject: subject.into(),
        date: Some(date),
        message_id: Some(mid.into()),
        references: refs.iter().map(|r| r.to_string()).collect(),
        from: vec![Address { name: from.into(), addr: format!("{}@example.org", from.to_lowercase()), name_looks_like_address: false }],
        preview: format!("{from} says {subject}"),
        ..Default::default()
    };
    Incoming { server_id: None, size: 100, internal_date: None, flags: 0, blob, full: true, summary }
}

#[test]
fn conversations_join_by_references_and_subject() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let a = s.add_account("priya@example.org").unwrap();
    let inbox = s.folder_id(a, "INBOX", Role::Inbox).unwrap();
    let sent = s.folder_id(a, "Sent", Role::Sent).unwrap();
    const DAY: i64 = 86_400;
    s.write(|w| {
        // A reply arrives before what it answers.
        w.put(inbox, a, 2, mail(&s, "Re: Q4 numbers", 2 * DAY, "b@x", &["a@x"], "Arun"))?;
        w.put(inbox, a, 1, mail(&s, "Q4 numbers", DAY, "a@x", &[], "Lena"))?;
        // Priya's own reply is in Sent.
        w.put(sent, a, 1, mail(&s, "Re: Q4 numbers", 3 * DAY, "c@x", &["a@x", "b@x"], "Priya"))?;
        // A reply with its references stripped finds it by subject...
        w.put(inbox, a, 3, mail(&s, "RE: q4  NUMBERS", 4 * DAY, "d@x", &[], "Sam"))?;
        // ...but not three weeks on, and not without a Re: prefix.
        w.put(inbox, a, 4, mail(&s, "Re: Q4 numbers", 30 * DAY, "e@x", &[], "Sam"))?;
        w.put(inbox, a, 5, mail(&s, "Lunch", 5 * DAY, "f@x", &[], "Lena"))?;
        w.put(inbox, a, 6, mail(&s, "Lunch", 6 * DAY, "g@x", &[], "Arun"))?;
        Ok(())
    }).unwrap();
    let c = s.conversations(inbox, 0, 10).unwrap();
    let rows: Vec<(&str, usize)> = c.iter().map(|c| (c.subject.as_str(), c.count)).collect();
    assert_eq!(rows, [("Re: Q4 numbers", 1), ("Lunch", 1), ("Lunch", 1), ("Q4 numbers", 4)]);
    let q4 = &c[3];
    assert_eq!(q4.people, ["Lena", "Arun", "Priya", "Sam"]);
    assert_eq!(q4.date, 4 * DAY);
    assert_eq!(q4.preview, "Sam says RE: q4  NUMBERS");
    assert!(q4.unread);
    // Listed in Sent too, with the whole thread.
    assert_eq!(s.conversation_count(sent).unwrap(), 1);
    let t = s.thread(q4.thread).unwrap();
    assert_eq!(t.iter().map(|m| m.message.summary.message_id.clone().unwrap()).collect::<Vec<_>>(), ["a@x", "b@x", "c@x", "d@x"]);
    // Views: conversations in the inbox, messages in Sent, then a choice.
    assert!(s.view(inbox).unwrap());
    assert!(!s.view(sent).unwrap());
    s.write(|w| w.set_view(sent, true)).unwrap();
    assert!(s.view(sent).unwrap());
    // Read on one copy: the conversation is read once all are.
    s.write(|w| { for u in [1, 2, 3] { w.set_flags(inbox, u, flag::SEEN)?; } w.set_flags(sent, 1, flag::SEEN).map(|_| ()) }).unwrap();
    assert!(!s.conversations(inbox, 3, 1).unwrap()[0].unread);
    // The newest goes: the thread moves back under its next newest; when
    // the last of it leaves the inbox it leaves the inbox's list.
    s.write(|w| w.remove(inbox, 3)).unwrap();
    let c = s.conversations(inbox, 0, 10).unwrap();
    assert_eq!(c.last().map(|c| (c.date, c.count)), Some((3 * DAY, 3)));
    s.write(|w| { w.remove(inbox, 1)?; w.remove(inbox, 2) }).unwrap();
    assert_eq!(s.conversation_count(inbox).unwrap(), 3);
    assert_eq!(s.conversation_count(sent).unwrap(), 1);
    s.write(|w| w.remove(sent, 1)).unwrap();
    assert_eq!(s.conversation_count(sent).unwrap(), 0);
}

#[test]
fn two_threads_that_meet_are_one() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let a = s.add_account("priya@example.org").unwrap();
    let inbox = s.folder_id(a, "INBOX", Role::Inbox).unwrap();
    s.write(|w| {
        w.put(inbox, a, 1, mail(&s, "Plan", 100, "a@x", &[], "Lena"))?;
        w.put(inbox, a, 2, mail(&s, "Other plan", 200, "z@x", &[], "Sam"))?;
        // Refers to both.
        w.put(inbox, a, 3, mail(&s, "Re: Plan", 300, "m@x", &["a@x", "z@x"], "Arun"))?;
        Ok(())
    }).unwrap();
    let c = s.conversations(inbox, 0, 10).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].count, 3);
}

#[test]
fn an_archive_leaves_the_list_at_once_and_undo_brings_it_back() {
    use mail_store::{OpKind, LEAVING};
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let a = s.add_account("priya@example.org").unwrap();
    let inbox = s.folder_id(a, "INBOX", Role::Inbox).unwrap();
    let archive = s.folder_id(a, "Archive", Role::Archive).unwrap();
    s.write(|w| { w.put(inbox, a, 10, inc(&s, "keep", 1_000, None, true))?; w.put(inbox, a, 11, inc(&s, "go", 2_000, None, true))?; Ok(()) }).unwrap();
    assert_eq!(s.conversation_count(inbox).unwrap(), 2);

    // Archive: gone from the list and the conversations at once, still known to sync.
    let op = s.write(|w| w.queue(inbox, 11, OpKind::Move { to: archive }, 100)).unwrap();
    assert_eq!(s.list(inbox, 0, 10).unwrap().iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["keep"]);
    assert_eq!(s.conversation_count(inbox).unwrap(), 1);
    assert_eq!(s.uids(inbox).unwrap(), [10, 11], "sync must not fetch it again");
    // A text arriving for it in the undo window doesn't bring it back.
    s.write(|w| w.put(inbox, a, 11, inc(&s, "go", 2_000, None, true)).map(|_| ())).unwrap();
    s.write(|w| w.set_flags(inbox, 11, flag::SEEN).map(|_| ())).unwrap();
    assert_eq!(s.count(inbox).unwrap(), 1);
    assert_eq!(s.at(inbox, 11).unwrap().unwrap().1 & LEAVING, LEAVING);
    // Not due yet; then due.
    assert!(s.ops_due(a, 99).unwrap().is_empty());
    assert_eq!(s.ops_due(a, 100).unwrap().len(), 1);
    // Undo: back in the list.
    assert!(s.write(|w| w.undo(op)).unwrap());
    assert_eq!(s.count(inbox).unwrap(), 2);
    assert_eq!(s.conversation_count(inbox).unwrap(), 2);
    assert!(s.ops_due(a, i64::MAX).unwrap().is_empty());
    assert!(!s.write(|w| w.undo(op)).unwrap(), "once");

    // Done on the server: the server's expunge then removes the location,
    // and the message with it.
    let op = s.write(|w| w.queue(inbox, 11, OpKind::Move { to: archive }, 0)).unwrap();
    s.write(|w| w.op_done(op, true)).unwrap();
    assert_eq!(s.count(inbox).unwrap(), 1);
    s.write(|w| w.remove(inbox, 11)).unwrap();
    assert_eq!(s.uids(inbox).unwrap(), [10]);

    // Read, refused by the server: back to unread.
    let op = s.write(|w| w.queue(inbox, 10, OpKind::Flags { add: flag::SEEN, remove: 0 }, 0)).unwrap();
    assert_eq!(s.at(inbox, 10).unwrap().unwrap().1, flag::SEEN);
    assert!(!s.conversations(inbox, 0, 10).unwrap()[0].unread);
    s.write(|w| w.op_done(op, false)).unwrap();
    assert_eq!(s.at(inbox, 10).unwrap().unwrap().1, 0);
    assert!(s.conversations(inbox, 0, 10).unwrap()[0].unread);
    // Moving to a folder of another account is refused.
    let b = s.add_account("work@example.org").unwrap();
    let other = s.folder_id(b, "Archive", Role::Archive).unwrap();
    assert!(s.write(|w| w.queue(inbox, 10, OpKind::Move { to: other }, 0)).is_err());
}

#[test]
fn calendar_delivery_survives_restart_hydration_and_racing_receipts() {
    use mail_schema::{Invite, InviteMethod, SenderCheck};
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path()).unwrap();
    let account = s.add_account("dev@fixture.invalid").unwrap();
    let inbox = s.folder_id(account, "INBOX", Role::Inbox).unwrap();
    let invitation = |seq, full| {
        let mut m = inc(&s, "Design", 100 + seq as i64, None, full);
        m.summary.invite = Some(Invite {
            method: InviteMethod::Request,
            uid: "design".into(),
            sequence: seq,
            title: "Design".into(),
            start: None,
            end: None,
            location: String::new(),
            organizer: "p@example.org".into(),
            attendees: 1,
            my_status: None,
            cancelled: false,
            repeats: false,
        });
        m
    };
    let original = s
        .write(|w| w.put(inbox, account, 1, invitation(1, true)))
        .unwrap();
    let later = s
        .write(|w| w.put(inbox, account, 2, invitation(3, false)))
        .unwrap();
    assert!(!s.index_calendar_page().unwrap());
    assert!(s.pending_calendar(0).unwrap().is_empty());
    let verified = SenderCheck::Verified {
        domain: "example.org".into(),
        by_provider: false,
    };
    s.write(|w| w.set_sender_check(later, verified.clone()))
        .unwrap();
    assert!(s.pending_calendar(0).unwrap().is_empty());
    let before_send = s.write(|w| w.calendar_revisions(account, "design", 1, "p@example.org")).unwrap();
    s.write(|w| before_send.recheck(w)).unwrap();
    let mut full = invitation(3, true);
    full.summary.sender_check = verified;
    s.write(|w| w.put(inbox, account, 2, full)).unwrap();
    assert!(matches!(s.write(|w| before_send.recheck(w)), Err(mail_store::Error::CalendarChanged)));
    let old = s.pending_calendar(0).unwrap()[0];
    assert_eq!(old.0, later);
    drop(s);
    let s = Store::open(d.path()).unwrap();
    assert_eq!(s.pending_calendar(0).unwrap(), [old]);
    s.write(|w| w.replay_calendar_revisions(original)).unwrap();
    s.write(|w| w.finish_calendar_delivery(old.0, old.1))
        .unwrap();
    let newest = s.pending_calendar(0).unwrap()[0];
    assert_eq!(newest.0, later);
    assert!(newest.1 > old.1);
    s.write(|w| w.finish_calendar_delivery(newest.0, newest.1))
        .unwrap();
    assert!(s.pending_calendar(0).unwrap().is_empty());
    assert!(s.write(|w| w.hold(inbox, 2, 1)).unwrap());
    s.write(|w| w.release(later)).unwrap();
    let released = s.pending_calendar(0).unwrap()[0];
    assert_eq!(released.0, later);
    s.write(|w| w.finish_calendar_delivery(later, released.1))
        .unwrap();
    let archive = s.folder_id(account, "Archive", Role::Archive).unwrap();
    let header = inc(&s, "Design", 103, None, false);
    s.write(|w| w.put_linked(archive, account, 3, header, Some(later)))
        .unwrap();
    assert_eq!(s.pending_calendar(0).unwrap()[0].0, later);
}
