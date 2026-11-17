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
    assert_eq!(flag::from_imap(&["\\Seen", "\\Answered", "$Junk"]), flag::SEEN | flag::ANSWERED);
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
