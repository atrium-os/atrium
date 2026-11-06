//! limend with a fake frescod and a fake portcullisd (limen.md §12 L1).

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use limen::catalogue::{self, SlotKind};
use limen::client::{Child, Error, Parent};
use limen::daemon::{serve, Compositor, Launcher, Shared};
use limen::proto::{refusal, ChildEvent, EmbedId, ParentEvent, WindowToken};

const APP: u32 = 1001;
const OTHER: u32 = 1002;
const ARCA_UID: u32 = 900;
const VIEWER_UID: u32 = 1500;
const W_APP: WindowToken = [1; 16];
const W_OTHER: WindowToken = [2; 16];
const W_ARCA: WindowToken = [3; 16];
const W_VIEWER: WindowToken = [4; 16];

/// frescod: window tokens → owner uids; records binds.
#[derive(Clone, Default)]
struct Fresco(Arc<Mutex<(HashMap<WindowToken, u32>, Vec<(WindowToken, WindowToken, SlotKind, Option<[f32; 4]>)>)>>);
impl Compositor for Fresco {
    fn bind(&mut self, p: WindowToken, c: WindowToken, k: SlotKind, r: Option<[f32; 4]>) -> Result<(u32, u32), u8> {
        let mut g = self.0.lock().unwrap();
        g.1.push((p, c, k, r));
        match (g.0.get(&p), g.0.get(&c)) {
            (Some(&a), Some(&b)) => Ok((a, b)),
            _ => Err(2),
        }
    }
}
impl Fresco {
    fn binds(&self) -> Vec<(WindowToken, WindowToken, SlotKind, Option<[f32; 4]>)> { self.0.lock().unwrap().1.clone() }
}

/// portcullisd: hands the child's channel to the test.
#[derive(Clone)]
struct Launch(mpsc::Sender<(EmbedId, UnixStream)>, Arc<Mutex<Vec<EmbedId>>>);
impl Launcher for Launch {
    fn launch(&mut self, app: &str, embed: EmbedId, ch: UnixStream) -> Result<u32, String> {
        assert_eq!(app, "org.example.viewer");
        self.0.send((embed, ch)).unwrap();
        Ok(VIEWER_UID)
    }
    fn end(&mut self, embed: EmbedId) { self.1.lock().unwrap().push(embed) }
}

struct World {
    shared: Arc<Shared>,
    fresco: Fresco,
    launched: mpsc::Receiver<(EmbedId, UnixStream)>,
    ended: Arc<Mutex<Vec<EmbedId>>>,
}

impl World {
    fn new() -> Self {
        let fresco = Fresco::default();
        {
            let mut g = fresco.0.lock().unwrap();
            g.0.insert(W_APP, APP);
            g.0.insert(W_OTHER, OTHER);
            g.0.insert(W_ARCA, ARCA_UID);
            g.0.insert(W_VIEWER, VIEWER_UID);
        }
        let (tx, rx) = mpsc::channel();
        let ended = Arc::new(Mutex::new(Vec::new()));
        let resolve = Box::new(|uid: u32| match uid {
            APP => Some(("ana".into(), "org.example.mail".into())),
            OTHER => Some(("ana".into(), "org.example.other".into())),
            ARCA_UID => Some(("ana".into(), catalogue::ARCA.into())),
            _ => None,
        });
        let defaults = Box::new(|_: &str, role: &str| (role == "doc-viewer").then(|| "org.example.viewer".to_string()));
        let shared = Shared::new(Box::new(fresco.clone()), Box::new(Launch(tx, ended.clone())), resolve, defaults);
        World { shared, fresco, launched: rx, ended }
    }

    fn connect(&self, uid: u32) -> aqueduct::Connection {
        let (a, b) = UnixStream::pair().unwrap();
        let sh = self.shared.clone();
        std::thread::spawn(move || { let _ = serve(aqueduct::Connection::wrap(b).unwrap(), uid, sh); });
        aqueduct::Connection::wrap(a).unwrap()
    }
    fn parent(&self, uid: u32) -> Parent { Parent::wrap(self.connect(uid)) }
    fn arca(&self) -> Child { provider(self.connect(ARCA_UID), "credential").unwrap() }
    fn spawned(&self) -> (EmbedId, Child) {
        let (e, s) = self.launched.recv_timeout(Duration::from_secs(2)).unwrap();
        (e, Child::wrap(aqueduct::Connection::wrap(s).unwrap()))
    }
}

fn provider(conn: aqueduct::Connection, role: &str) -> Result<Child, Error> {
    let mut c = Child::wrap(conn);
    c.provide_role(role)?;
    Ok(c)
}

fn next_p(p: &mut Parent) -> ParentEvent { p.poll(Duration::from_secs(2)).unwrap().expect("a parent event") }
fn next_c(c: &mut Child) -> ChildEvent { c.poll(Duration::from_secs(2)).unwrap().expect("a child event") }
fn refused(r: Result<impl std::fmt::Debug, Error>) -> u8 {
    match r { Err(Error::Refused(x)) => x, other => panic!("expected a refusal, got {other:?}") }
}

const RECT: Option<[f32; 4]> = Some([10.0, 20.0, 240.0, 36.0]);

#[test]
fn credential_embed_attaches_relays_and_detaches() {
    let w = World::new();
    let mut arca = w.arca();
    let mut app = w.parent(APP);
    let e = app.request("credential", W_APP).unwrap();
    assert_eq!(next_c(&mut arca), ChildEvent::Attach { embed: e, role: "credential".into(), parent_app: "org.example.mail".into() });
    app.place(e, RECT).unwrap();
    // Not attached until the child's window is bound.
    assert!(app.poll(Duration::from_millis(50)).unwrap().is_none());
    arca.ready(e, W_ARCA).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: e });
    assert_eq!(w.fresco.binds().last(), Some(&(W_APP, W_ARCA, SlotKind::Credential, RECT)));

    app.send(e, b"cfg".to_vec()).unwrap();
    assert_eq!(next_c(&mut arca), ChildEvent::Message { embed: e, bytes: b"cfg".to_vec() });
    arca.emit(e, b"filled".to_vec()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed: e, bytes: b"filled".to_vec() });

    // Hidden: released, both told.
    app.place(e, None).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Suspended { embed: e });
    assert_eq!(next_c(&mut arca), ChildEvent::Suspended { embed: e });
    assert_eq!(w.fresco.binds().last().unwrap().3, None);
    app.place(e, RECT).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Resumed { embed: e });

    app.detach(e).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Detached { embed: e });
    assert_eq!(next_c(&mut arca), ChildEvent::Resumed { embed: e });
    assert_eq!(next_c(&mut arca), ChildEvent::Detaching { embed: e });
    assert_eq!(w.shared.embeds(), 0);
}

#[test]
fn only_the_catalogues_service_provides_a_system_role() {
    let w = World::new();
    // An app claiming to be Arca.
    let r = provider(w.connect(APP), "credential");
    assert_eq!(refused(r.map(|_| ())), refusal::NOT_PROVIDER);
    // Arca can't take a user role either.
    let r = provider(w.connect(ARCA_UID), "doc-viewer");
    assert_eq!(refused(r.map(|_| ())), refusal::NOT_PROVIDER);
    // With no provider up, a system role has none.
    let mut app = w.parent(APP);
    assert_eq!(refused(app.request("credential", W_APP)), refusal::NO_PROVIDER);
    assert_eq!(refused(app.request("nonsense", W_APP)), refusal::UNKNOWN_ROLE);
    // An unregistered process isn't an app.
    let mut stranger = w.parent(4242);
    assert_eq!(refused(stranger.request("doc-viewer", W_APP)), refusal::NOT_AN_APP);
}

#[test]
fn a_parent_cannot_embed_into_a_window_it_does_not_own() {
    let w = World::new();
    let mut arca = w.arca();
    let mut app = w.parent(APP);
    // The token is someone else's window (frescod says OTHER owns it).
    let e = app.request("credential", W_OTHER).unwrap();
    let _ = next_c(&mut arca);
    app.place(e, RECT).unwrap();
    arca.ready(e, W_ARCA).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::ChildLost { embed: e });
    assert_eq!(w.shared.embeds(), 0);
    // And the child can't name a window that isn't its own.
    let e = app.request("credential", W_APP).unwrap();
    let _ = next_c(&mut arca);
    app.place(e, RECT).unwrap();
    arca.ready(e, W_VIEWER).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::ChildLost { embed: e });
}

#[test]
fn embeds_are_only_their_owners() {
    let w = World::new();
    let mut arca = w.arca();
    let mut app = w.parent(APP);
    let mut other = w.parent(OTHER);
    let e = app.request("credential", W_APP).unwrap();
    let _ = next_c(&mut arca);
    assert_eq!(refused(other.place(e, RECT)), refusal::NOT_YOURS);
    assert_eq!(refused(other.send(e, vec![1])), refusal::NOT_YOURS);
    assert_eq!(refused(other.detach(e)), refusal::NOT_YOURS);
    // A spawned child can't speak for an embed that isn't its own.
    let v = app.request("doc-viewer", W_APP).unwrap();
    let (ve, mut viewer) = w.spawned();
    assert_eq!(ve, v);
    let _ = next_c(&mut viewer);
    assert_eq!(refused(viewer.emit(e, vec![1])), refusal::NOT_YOURS);
    assert_eq!(refused(viewer.ready(e, W_VIEWER)), refusal::NOT_YOURS);
}

#[test]
fn messages_are_capped_in_size_and_rate() {
    let w = World::new();
    let mut arca = w.arca();
    let mut app = w.parent(APP);
    let e = app.request("credential", W_APP).unwrap();
    let _ = next_c(&mut arca);
    assert_eq!(refused(app.send(e, vec![0; catalogue::MAX_MESSAGE + 1])), refusal::TOO_BIG);
    assert_eq!(refused(arca.emit(e, vec![0; catalogue::MAX_MESSAGE + 1])), refusal::TOO_BIG);
    let mut refusedn = 0;
    for _ in 0..catalogue::MAX_MESSAGES_PER_SEC + 10 {
        if arca.emit(e, vec![1]).is_err() { refusedn += 1; }
    }
    assert!(refusedn >= 1, "over the rate is dropped");
    let mut dropped = 0;
    while let Some(ev) = app.poll(Duration::from_millis(100)).unwrap() {
        if let ParentEvent::Dropped { .. } = ev { dropped += 1; }
    }
    assert_eq!(dropped, 1, "told once");
}

#[test]
fn a_spawned_child_crash_is_child_lost_and_a_parent_exit_ends_its_children() {
    let w = World::new();
    let mut app = w.parent(APP);
    let e = app.request("doc-viewer", W_APP).unwrap();
    let (_, mut viewer) = w.spawned();
    assert_eq!(next_c(&mut viewer), ChildEvent::Attach { embed: e, role: "doc-viewer".into(), parent_app: "org.example.mail".into() });
    app.place(e, RECT).unwrap();
    viewer.ready(e, W_VIEWER).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: e });
    assert_eq!(w.fresco.binds().last().unwrap().2, SlotKind::Plain);
    drop(viewer); // crash
    assert_eq!(next_p(&mut app), ParentEvent::ChildLost { embed: e });
    std::thread::sleep(Duration::from_millis(catalogue::DETACH_GRACE_MS + 200));
    assert_eq!(*w.ended.lock().unwrap(), vec![e]);
    assert_eq!(w.fresco.binds().last().unwrap().3, None, "slot released");

    let e2 = app.request("doc-viewer", W_APP).unwrap();
    let (_, mut viewer) = w.spawned();
    let _ = next_c(&mut viewer);
    drop(app); // parent exits
    assert_eq!(next_c(&mut viewer), ChildEvent::Detaching { embed: e2 });
    std::thread::sleep(Duration::from_millis(catalogue::DETACH_GRACE_MS + 200));
    assert!(w.ended.lock().unwrap().contains(&e2));
    assert_eq!(w.shared.embeds(), 0);
}

#[test]
fn at_most_sixteen_embeds_per_parent() {
    let w = World::new();
    let _arca = w.arca();
    let mut app = w.parent(APP);
    for _ in 0..catalogue::MAX_EMBEDS_PER_PARENT {
        app.request("credential", W_APP).unwrap();
    }
    assert_eq!(refused(app.request("credential", W_APP)), refusal::TOO_MANY);
}
