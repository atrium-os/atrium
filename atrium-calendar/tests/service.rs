use atrium_calendar::{
    wire::{call, page, Request},
    CalendarView, Entry, Kind, Span,
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(dir: &Path, publisher: &str) -> (Daemon, PathBuf) {
    let socket = dir.join("calendar.sock");
    let mut child = Daemon(
        Command::new(
            std::env::var_os("FASTI_TEST_BIN")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_fastid").into()),
        )
        .env("FASTI_SOCKET", &socket)
        .env("FASTI_REGISTRY", dir.join("registry"))
        .env("FASTI_USER", "mina")
        .env("FASTI_PERSONA", "work")
        .env("FASTI_STORE", dir)
        .env("FASTI_PUBLISHER", publisher)
        .spawn()
        .unwrap(),
    );
    let limit = Instant::now() + Duration::from_secs(4);
    loop {
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none() && Instant::now() < limit,
            "Fasti failed to start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    (child, socket)
}

#[test]
fn socket_enforces_identity_and_retains_the_accepted_revision() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let dir = temp.path();
    let uid = fs::metadata(dir).unwrap().uid();
    let registry = dir.join("registry");
    let publisher = "ab".repeat(32);
    let register = |user: &str, app: &str, signer: &str, suffix: &str| {
        fs::write(&registry, format!("{uid} {user} {app} {signer}{suffix}\n")).unwrap();
    };
    register("mina", "org.atrium.calendar", &publisher, " persona=work");
    let (daemon, socket) = start(dir, &publisher);
    let date = "2026-11-24".parse().unwrap();
    let query = Request::Day { date };
    let resume = Request::Browse { date, view: None };
    let set_week = Request::SetOpeningView {
        revision: 1,
        opening_view: CalendarView::Week,
        date,
        view: CalendarView::Today,
    };
    assert_eq!(page(&socket, &resume).unwrap().view, CalendarView::Today);
    assert_eq!(call(&socket, &query).unwrap().revision, 0);
    let event = Entry {
        import_hash: None,
        invitation: None,
        excluded: Vec::new(),
        zone: None,
        source: None,
        id: Uuid::new_v4(),
        date,
        kind: Kind::Appointment,
        title: "Budget review".into(),
        span: Span {
            start: 840,
            end: 900,
        },
    };
    let saved = page(
        &socket,
        &Request::Add {
            revision: 0,
            entry: event.clone(),
        },
    )
    .unwrap();
    assert_eq!(saved.entries[0].id, event.id);
    let undo = Request::Undo {
        receipt: saved.undo.unwrap(),
    };
    assert!(page(&socket, &query).unwrap().undo.is_none());
    let search = || {
        atrium_calendar::wire::search(
            &socket,
            atrium_calendar::SearchQuery {
                text: "budget".into(),
                start: date,
                end: date,
            },
            0,
            None,
        )
    };
    let result = search().unwrap();
    assert_eq!(result.hits[0].id, event.id);
    let open_hit = Request::OpenSearchHit {
        revision: result.revision,
        date,
        id: event.id,
    };
    assert_eq!(page(&socket, &open_hit).unwrap().entries[0].id, event.id);
    for (user, app, signer, suffix) in [
        ("mina", "org.atrium.calendar", publisher.as_str(), ""),
        (
            "mina",
            "org.atrium.mail",
            publisher.as_str(),
            " persona=work",
        ),
        ("mina", "org.atrium.calendar", "", " persona=work"),
        (
            "mina",
            "org.atrium.calendar",
            &"cd".repeat(32),
            " persona=work",
        ),
        (
            "someone",
            "org.atrium.calendar",
            publisher.as_str(),
            " persona=work",
        ),
        (
            "mina",
            "org.atrium.calendar",
            publisher.as_str(),
            " persona=work document=2",
        ),
    ] {
        register(user, app, signer, suffix);
        assert!(call(&socket, &query).is_err());
        assert!(search().is_err());
        assert!(page(&socket, &open_hit).is_err());
        assert!(page(&socket, &resume).is_err());
        assert!(page(&socket, &undo).is_err());
        assert!(page(&socket, &set_week).is_err());
        assert!(page(
            &socket,
            &Request::Change {
                revision: 1,
                id: event.id,
                shown: date,
                scope: atrium_calendar::EditScope::ThisEvent,
                operation: atrium_calendar::wire::EventChange::Delete,
            }
        )
        .is_err());
        assert!(call(
            &socket,
            &Request::Add {
                revision: 1,
                entry: Entry {
                    id: Uuid::new_v4(),
                    ..event.clone()
                }
            }
        )
        .is_err());
    }
    register("mina", "org.atrium.calendar", &publisher, " persona=work");
    let mut stream = UnixStream::connect(&socket).unwrap();
    let big = aqueduct::Header::new(
        aqueduct::classes::CLASS_CALENDAR,
        1,
        aqueduct::flag::RESPONSE_EXPECTED,
        4097,
    );
    stream.write_all(&big.encode()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    let (_, bytes) = aqueduct::envelope::read_message_alloc(&mut &reply[..]).unwrap();
    assert!(
        serde_json::from_slice::<Result<atrium_calendar::Day, String>>(&bytes)
            .unwrap()
            .is_err()
    );
    drop(daemon);
    let (_daemon, socket) = start(dir, &publisher);
    assert!(page(&socket, &undo).unwrap_err().contains("expired"));
    let reloaded = call(&socket, &query).unwrap();
    assert_eq!(reloaded.revision, 1);
    assert_eq!(reloaded.entries.len(), 1);
    assert_eq!(reloaded.entries[0].id, event.id);
    let saved = page(&socket, &set_week).unwrap();
    assert_eq!(saved.view, CalendarView::Today);
    assert_eq!(saved.opening_view, CalendarView::Week);
    assert_eq!(page(&socket, &resume).unwrap().view, CalendarView::Week);
    assert!(page(&socket, &set_week).is_err());
}

#[test]
fn broker_deposit_and_window_query_use_separate_operations() {
    use aqueduct::{classes::CLASS_CALENDAR, envelope, flag, Header};
    use atrium_calendar::{store::Store, wire};
    use portcullis_ipc::calendar::BACKGROUND_OP;

    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let query = serde_json::to_vec(&Request::Today { view: None }).unwrap();
    let request = |op| {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let header = Header::new(
            CLASS_CALENDAR,
            op,
            flag::RESPONSE_EXPECTED,
            query.len() as u32,
        );
        envelope::write_message(&mut sender, header, &query).unwrap();
        (sender, receiver)
    };
    let (_client, mut broker) = request(1);
    assert_eq!(
        wire::serve_deposit(&mut broker, &mut store)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    let (mut client, mut window) = request(BACKGROUND_OP);
    wire::serve(&mut window, &mut store).unwrap();
    let (_, bytes) = envelope::read_message_alloc(&mut client).unwrap();
    assert!(
        serde_json::from_slice::<Result<atrium_calendar::CalendarPage, String>>(&bytes)
            .unwrap()
            .is_err()
    );
}
