//! limend with a fake frescod and a fake portcullisd (limen.md §12 L1).

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use limen::catalogue::{self, SlotKind};
use limen::client::{Child, Error, Parent};
use limen::daemon::{serve, Attribution, Compositor, Launcher, Shared};
use limen::proto::{refusal, ChildEvent, EmbedId, ParentEvent, WindowToken};

const APP: u32 = 1001;
const OTHER: u32 = 1002;
const ARCA_UID: u32 = 900;
const VIEWER_UID: u32 = 1500;
const PLAYER_UID: u32 = 1600;
const SCRINIUM_UID: u32 = 1700;
const SHARE_UID: u32 = 1800;
const NOTES_UID: u32 = 1900;
const W_SHEET: WindowToken = [8; 16];
const W_NOTES: WindowToken = [9; 16];
const PRELUM_UID: u32 = 2000;
const W_PRINT: WindowToken = [10; 16];
const W_PICKER: WindowToken = [7; 16];
const W_APP: WindowToken = [1; 16];
const W_OTHER: WindowToken = [2; 16];
const W_ARCA: WindowToken = [3; 16];
const W_VIEWER: WindowToken = [4; 16];
const CURIA_UID: u32 = 2100;
const W_CURIA: WindowToken = [11; 16];

/// frescod: window tokens → owner uids; records binds.
#[derive(Clone, Default)]
struct Fresco(Arc<Mutex<(HashMap<WindowToken, u32>, Vec<(WindowToken, WindowToken, SlotKind, Option<[f32; 4]>)>, Vec<(u32, UnixStream)>)>>, Arc<Mutex<Presses>>);
/// Windows that saw a press lately, and the slots released for PiP.
#[derive(Default)]
struct Presses { pressed: Vec<WindowToken>, released: Vec<WindowToken> }
impl Compositor for Fresco {
    fn bind(&mut self, p: WindowToken, c: WindowToken, k: SlotKind, r: Option<[f32; 4]>, _z: u8) -> Result<(u32, u32), u8> {
        let mut g = self.0.lock().unwrap();
        g.1.push((p, c, k, r));
        match (g.0.get(&p), g.0.get(&c)) {
            (Some(&a), Some(&b)) => Ok((a, b)),
            _ => Err(2),
        }
    }
    fn embed_connection(&mut self, owner: u32) -> Result<UnixStream, u8> {
        // Stands for frescod's embed-only connection; the other end is kept
        // so the child's end stays open.
        let (a, b) = UnixStream::pair().unwrap();
        self.0.lock().unwrap().2.push((owner, b));
        Ok(a)
    }
    fn pressed_within(&mut self, w: WindowToken, ms: u32) -> bool {
        assert_eq!(ms, catalogue::PIP_GESTURE_MS);
        self.1.lock().unwrap().pressed.contains(&w)
    }
    fn spend_press(&mut self, w: WindowToken, ms: u32) -> bool {
        assert_eq!(ms, catalogue::ISLAND_PRESS_MS);
        let mut g = self.1.lock().unwrap();
        let before = g.pressed.len();
        g.pressed.retain(|&p| p != w);
        g.pressed.len() != before
    }
    fn release(&mut self, _p: WindowToken, c: WindowToken) -> Result<(), u8> {
        self.1.lock().unwrap().released.push(c);
        Ok(())
    }
}
impl Fresco {
    fn binds(&self) -> Vec<(WindowToken, WindowToken, SlotKind, Option<[f32; 4]>)> { self.0.lock().unwrap().1.clone() }
}

/// portcullisd: hands the child's channel to the test.
#[derive(Clone)]
struct Launch(mpsc::Sender<(EmbedId, UnixStream)>, Arc<Mutex<Vec<EmbedId>>>, Arc<std::sync::atomic::AtomicBool>);
impl Launcher for Launch {
    fn launch(&mut self, app: &str, embed: EmbedId, ch: UnixStream) -> Result<u32, String> {
        assert!(["org.example.viewer", "org.example.player", "org.example.map", "org.example.notes", "org.atrium.navigator.mail-page", catalogue::NAVIGATOR_ENGINE].contains(&app), "{app}");
        self.0.send((embed, ch)).unwrap();
        Ok(match app { "org.example.player" => PLAYER_UID, "org.example.notes" => NOTES_UID, _ => VIEWER_UID })
    }
    fn launch_island(&mut self, embed: EmbedId, ch: UnixStream, _: u32, mut fetch: UnixStream, memory_mb: Option<u64>) -> Result<u32, String> {
        use std::io::Write;
        // The marker the test reads: the embed, then the limit asked (0: the manifest's).
        fetch.write_all(&embed.to_le_bytes()).unwrap();
        fetch.write_all(&memory_mb.unwrap_or(0).to_le_bytes()).unwrap();
        self.launch(catalogue::NAVIGATOR_ENGINE, embed, ch)
    }
    fn launch_island_standby(&mut self, embed: EmbedId, ch: UnixStream, _: u32) -> Result<u32, String> {
        if !self.2.load(std::sync::atomic::Ordering::SeqCst) { return Err("off".into()) }
        self.launch(catalogue::NAVIGATOR_ENGINE, embed, ch)
    }

    /// A share across personas: this stand-in for portcullisd has the
    /// person's `share` switch on into Work only.
    fn launch_in(&mut self, app: &str, embed: EmbedId, ch: UnixStream, _parent: u32, persona: &str) -> Result<u32, String> {
        if persona != "work" {
            return Err(format!("sharing into {persona} is off"));
        }
        self.launch(app, embed, ch)
    }
    /// A settings pane: this stand-in for portcullisd starts one in the
    /// person's Personal or Work, and has no School.
    fn launch_settings(&mut self, app: &str, embed: EmbedId, ch: UnixStream, parent: u32, persona: &str) -> Result<u32, String> {
        assert_eq!(parent, CURIA_UID);
        if !["personal", "work"].contains(&persona) {
            return Err(format!("no persona {persona}"));
        }
        self.launch(app, embed, ch)
    }
    fn end(&mut self, embed: EmbedId) { self.1.lock().unwrap().push(embed) }
}

struct World {
    shared: Arc<Shared>,
    fresco: Fresco,
    launched: mpsc::Receiver<(EmbedId, UnixStream)>,
    ended: Arc<Mutex<Vec<EmbedId>>>,
    standby: Arc<std::sync::atomic::AtomicBool>,
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
            g.0.insert(W_PICKER, SCRINIUM_UID);
            g.0.insert(W_SHEET, SHARE_UID);
            g.0.insert(W_NOTES, NOTES_UID);
            g.0.insert(W_PRINT, PRELUM_UID);
            g.0.insert(W_CURIA, CURIA_UID);
        }
        let (tx, rx) = mpsc::channel();
        let ended = Arc::new(Mutex::new(Vec::new()));
        let resolve = Box::new(|uid: u32| match uid {
            APP => Some(("ana".into(), "org.example.mail".into())),
            4101 | 4102 => Some(("ana".into(), "org.atrium.navigator".into())),
            4111 | 4112 => Some(("ana".into(), "org.atrium.mail".into())),
            4113 => Some(("ben".into(), "org.atrium.mail".into())),
            OTHER => Some(("ana".into(), "org.example.other".into())),
            ARCA_UID => Some(("ana".into(), catalogue::ARCA.into())),
            SCRINIUM_UID => Some(("ana".into(), catalogue::SCRINIUM.into())),
            SHARE_UID => Some(("ana".into(), catalogue::SHARE.into())),
            PRELUM_UID => Some(("ana".into(), catalogue::PRELUM.into())),
            CURIA_UID => Some(("ana".into(), catalogue::CURIA.into())),
            _ => None,
        });
        let defaults = Box::new(|_: &str, role: &str| match role {
            "doc-viewer" => Some("org.example.viewer".to_string()),
            "media-player" => Some("org.example.player".to_string()),
            "map" => Some("org.example.map".to_string()),
            _ => None,
        });
        let standby = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shared = Shared::new(Box::new(fresco.clone()), Box::new(Launch(tx, ended.clone(), standby.clone())), resolve, defaults);
        // The mail app runs in Work (personas.md §3), everyone else in Personal.
        shared.set_persona_lookup(Box::new(|uid: u32| (uid == APP).then(|| "work".to_string())));
        World { shared, fresco, launched: rx, ended, standby }
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
        // The first thing on the channel is its embed-only frescod connection.
        let (c, _fresco) = Child::embedded_on(s).unwrap();
        (e, c)
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
    assert_eq!(next_c(&mut arca), ChildEvent::Attach { embed: e, role: "credential".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
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
fn a_doc_viewer_returns_thumbnail_pixels_without_a_placed_slot() {
    use limen::doc_viewer::{ToChild, ToParent, THUMBNAIL_EDGE, THUMBNAIL_RGBA_MAX};
    use std::os::fd::AsFd;

    let w = World::new();
    let mut app = w.parent(APP);
    let embed = app.request("doc-viewer", W_APP).unwrap();
    let (_, mut viewer) = w.spawned();
    assert!(matches!(next_c(&mut viewer), ChildEvent::Attach { embed: id, .. } if id == embed));
    viewer.ready(embed, W_VIEWER).unwrap();

    let path = std::env::temp_dir().join(format!("limen-thumbnail-{}", std::process::id()));
    std::fs::write(&path, b"encoded picture").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let request = ToChild::MakeThumbnail.encode();
    app.send_fd(embed, request.clone(), file.as_fd()).unwrap();
    assert_eq!(next_c(&mut viewer), ChildEvent::MessageFd { embed, bytes: request });
    assert!(viewer.take_fd().is_some());

    let reply = ToParent::Thumbnail {
        width: THUMBNAIL_EDGE, height: THUMBNAIL_EDGE, rgba: vec![8; THUMBNAIL_RGBA_MAX],
    }.encode();
    viewer.emit(embed, reply.clone()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed, bytes: reply });
    assert!(w.fresco.binds().is_empty());
}

#[test]
fn a_spawned_child_crash_is_child_lost_and_a_parent_exit_ends_its_children() {
    let w = World::new();
    let mut app = w.parent(APP);
    let e = app.request("doc-viewer", W_APP).unwrap();
    let (_, mut viewer) = w.spawned();
    assert_eq!(next_c(&mut viewer), ChildEvent::Attach { embed: e, role: "doc-viewer".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
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
fn each_web_island_launches_its_fixed_worker_with_an_embed_connection() {
    let w = World::new();
    let mut app = w.parent(APP);
    let mut children = Vec::new();
    for _ in 0..2 {
        let embed = app.request("web-island", W_APP).unwrap();
        let (instance, mut child) = w.spawned();
        assert_eq!(instance, embed);
        assert!(matches!(next_c(&mut child), ChildEvent::Attach { role, .. } if role == "web-island"));
        children.push(child);
    }
    assert_eq!(w.fresco.0.lock().unwrap().2.len(), 2);
}

#[test]
fn a_spawned_child_gets_an_embed_only_frescod_connection_for_its_own_uid() {
    let w = World::new();
    let mut app = w.parent(APP);
    let _ = app.request("doc-viewer", W_APP).unwrap();
    let _ = w.spawned();
    let owners: Vec<u32> = w.fresco.0.lock().unwrap().2.iter().map(|(u, _)| *u).collect();
    assert_eq!(owners, vec![VIEWER_UID], "frescod is asked for one, owned by the child, never by limend");
}

#[test]
fn a_fixed_system_uid_replaces_the_registry_for_its_provider() {
    let w = World::new();
    w.shared.set_system_uid(catalogue::ARCA, 65021);
    // The registry's "Arca" is no longer enough...
    assert_eq!(refused(provider(w.connect(ARCA_UID), "credential").map(|_| ())), refusal::NOT_PROVIDER);
    // ...only arca-ui's uid, which has no registry entry at all.
    assert!(provider(w.connect(65021), "credential").is_ok());
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

/// choragusd: issues a ticket only for a parent that holds audio (APP).
#[derive(Clone, Default)]
struct Tickets(Arc<Mutex<(Vec<(u32, u32)>, Vec<[u8; 16]>)>>);
impl Attribution for Tickets {
    fn issue(&mut self, child: u32, parent: u32) -> Option<[u8; 16]> {
        self.0.lock().unwrap().0.push((child, parent));
        (parent == APP).then_some([child as u8; 16])
    }
    fn cancel(&mut self, t: [u8; 16]) { self.0.lock().unwrap().1.push(t) }
}

/// Tickets are issued and cancelled off limend's state lock, on a thread of
/// their own: wait (bounded) for `done` to hold.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done() {
        assert!(std::time::Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn an_embedded_player_gets_an_audio_ticket_that_dies_with_the_embed() {
    let w = World::new();
    let tickets = Tickets::default();
    w.shared.set_attribution(Box::new(tickets.clone()));
    let mut app = w.parent(APP);
    let e = app.request("media-player", W_APP).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { embed, .. } if embed == e));
    assert_eq!(next_c(&mut player), ChildEvent::AudioTicket { embed: e, ticket: [PLAYER_UID as u8; 16] });
    // Asked for the player's uid (from limend's own launch) under the parent's.
    assert_eq!(tickets.0.lock().unwrap().0, vec![(PLAYER_UID, APP)]);
    app.detach(e).unwrap();
    eventually("the cancel", || !tickets.0.lock().unwrap().1.is_empty());
    assert_eq!(tickets.0.lock().unwrap().1, vec![[PLAYER_UID as u8; 16]], "cancelled when the embed ends");

    // A parent choragusd refuses (no audio): the player gets no ticket.
    let mut other = w.parent(OTHER);
    let e = other.request("media-player", W_OTHER).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { .. }));
    eventually("the refused issue", || tickets.0.lock().unwrap().0.len() == 2);
    other.detach(e).unwrap();
    assert_eq!(next_c(&mut player), ChildEvent::Detaching { embed: e }, "no AudioTicket in between");
    // Roles that don't play sound never ask.
    let _ = app.request("doc-viewer", W_APP).unwrap();
    let _ = w.spawned();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(tickets.0.lock().unwrap().0.len(), 2);
}

/// choragusd that takes its time (absent: limend's connect waits 2 s).
struct Slow(std::time::Duration, Tickets);
impl Attribution for Slow {
    fn issue(&mut self, child: u32, parent: u32) -> Option<[u8; 16]> {
        std::thread::sleep(self.0);
        self.1.issue(child, parent)
    }
    fn cancel(&mut self, t: [u8; 16]) { self.1.cancel(t) }
}

#[test]
fn a_slow_audio_service_delays_neither_the_embed_nor_other_requests() {
    let w = World::new();
    let tickets = Tickets::default();
    w.shared.set_attribution(Box::new(Slow(std::time::Duration::from_millis(1500), tickets.clone())));
    let mut app = w.parent(APP);
    let started = std::time::Instant::now();
    let e = app.request("media-player", W_APP).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { embed, .. } if embed == e));
    // limend's state is free while choragusd thinks: another request goes through.
    let _ = app.request("doc-viewer", W_APP).unwrap();
    let _ = w.spawned();
    assert!(started.elapsed() < std::time::Duration::from_millis(1000),
        "the embed waited on the audio service ({:?})", started.elapsed());
    // The ticket still arrives, after Attach.
    assert_eq!(next_c(&mut player), ChildEvent::AudioTicket { embed: e, ticket: [PLAYER_UID as u8; 16] });
}

#[test]
fn a_ticket_for_an_embed_that_ended_meanwhile_is_cancelled() {
    let w = World::new();
    let tickets = Tickets::default();
    w.shared.set_attribution(Box::new(Slow(std::time::Duration::from_millis(300), tickets.clone())));
    let mut app = w.parent(APP);
    let e = app.request("media-player", W_APP).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { .. }));
    app.detach(e).unwrap();
    assert_eq!(next_c(&mut player), ChildEvent::Detaching { embed: e });
    eventually("the late ticket's cancel", || !tickets.0.lock().unwrap().1.is_empty());
    assert_eq!(tickets.0.lock().unwrap().1, vec![[PLAYER_UID as u8; 16]]);
}

#[test]
fn a_player_gets_a_read_only_file_or_pipe_with_a_message_and_nothing_else() {
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsFd;
    let w = World::new();
    let mut app = w.parent(APP);
    let e = app.request("media-player", W_APP).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { .. }));

    // A regular file, read-only: delivered with the message.
    let path = std::env::temp_dir().join(format!("limen-fd-{}", std::process::id()));
    std::fs::write(&path, b"media bytes").unwrap();
    let ro = std::fs::File::open(&path).unwrap();
    app.send_fd(e, b"load".to_vec(), ro.as_fd()).unwrap();
    assert_eq!(next_c(&mut player), ChildEvent::MessageFd { embed: e, bytes: b"load".to_vec() });
    let mut got = std::fs::File::from(player.take_fd().expect("the descriptor"));
    let mut s = String::new();
    got.read_to_string(&mut s).unwrap();
    assert_eq!(s, "media bytes");
    got.rewind().unwrap();
    assert!(got.write_all(b"x").is_err(), "it stays read-only");

    // A pipe's read end.
    let (r, mut wr) = std::io::pipe().unwrap();
    wr.write_all(b"piped").unwrap();
    drop(wr);
    app.send_fd(e, b"load".to_vec(), r.as_fd()).unwrap();
    assert!(matches!(next_c(&mut player), ChildEvent::MessageFd { .. }));
    let mut s = String::new();
    std::fs::File::from(player.take_fd().unwrap()).read_to_string(&mut s).unwrap();
    assert_eq!(s, "piped");

    // A socket, a writable file, a pipe's write end: refused, nothing sent.
    let (sock, _peer) = UnixStream::pair().unwrap();
    assert_eq!(refused(app.send_fd(e, b"load".to_vec(), sock.as_fd())), refusal::BAD_FD);
    let rw = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    assert_eq!(refused(app.send_fd(e, b"load".to_vec(), rw.as_fd())), refusal::BAD_FD);
    let (_r2, w2) = std::io::pipe().unwrap();
    if cfg!(target_os = "freebsd") {
        // Pipes are bidirectional there: either end goes, narrowed to read.
        app.send_fd(e, b"load".to_vec(), w2.as_fd()).unwrap();
        assert!(matches!(next_c(&mut player), ChildEvent::MessageFd { .. }));
        let mut end = std::fs::File::from(player.take_fd().unwrap());
        assert!(end.write_all(b"x").is_err(), "narrowed: it can't write");
    } else {
        assert_eq!(refused(app.send_fd(e, b"load".to_vec(), w2.as_fd())), refusal::BAD_FD);
    }
    let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
    assert_eq!(refused(app.send_fd(e, b"load".to_vec(), dir.as_fd())), refusal::BAD_FD);
    // Plain messages still go through, and no descriptor came with them.
    app.send(e, b"play".to_vec()).unwrap();
    assert_eq!(next_c(&mut player), ChildEvent::Message { embed: e, bytes: b"play".to_vec() });
    assert!(player.take_fd().is_none());

    // A role that takes no descriptor, and someone else's embed.
    let v = app.request("map", W_APP).unwrap();
    let _map = w.spawned();
    assert_eq!(refused(app.send_fd(v, b"x".to_vec(), ro.as_fd())), refusal::MALFORMED);
    let mut other = w.parent(OTHER);
    assert_eq!(refused(other.send_fd(e, b"x".to_vec(), ro.as_fd())), refusal::NOT_YOURS);
    let _ = std::fs::remove_file(&path);
}

const W_PLAYER: WindowToken = [5; 16];
const W_PLAYER2: WindowToken = [6; 16];

/// A player embedded in APP's window, its slot placed and its window bound.
fn bound_player(w: &World, app: &mut Parent, window: WindowToken) -> (EmbedId, Child) {
    w.fresco.0.lock().unwrap().0.insert(window, PLAYER_UID);
    let e = app.request("media-player", W_APP).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { .. }));
    app.place(e, RECT).unwrap();
    player.ready(e, window).unwrap();
    assert_eq!(next_p(app), ParentEvent::Attached { embed: e });
    (e, player)
}

#[test]
fn pip_needs_a_real_press_and_leaves_the_slot_for_forum_and_comes_back() {
    let w = World::new();
    let mut app = w.parent(APP);
    let (e, mut player) = bound_player(&w, &mut app, W_PLAYER);

    // No press lately: refused, whatever the player says.
    assert_eq!(refused(player.pip(e, true, [16, 9])), refusal::NO_PIP);
    // A press on the page (requestPictureInPicture in a click handler).
    w.fresco.1.lock().unwrap().pressed.push(W_APP);
    assert_eq!(refused(player.pip(e, true, [0, 9])), refusal::NO_PIP, "a sane aspect");
    player.pip(e, true, [16, 9]).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: e, on: true });
    assert_eq!(next_c(&mut player), ChildEvent::Pip { embed: e, on: true });
    // Released with keep_shown: frescod marks it a PiP window for forum-wm.
    assert_eq!(w.fresco.1.lock().unwrap().released, vec![W_PLAYER]);

    // The tab goes to the background: no Suspended while in PiP.
    let binds = w.fresco.binds().len();
    app.set_visible(e, false).unwrap();
    app.place(e, Some([0.0, 0.0, 320.0, 180.0])).unwrap();
    assert!(app.poll(Duration::from_millis(50)).unwrap().is_none());
    assert_eq!(w.fresco.binds().len(), binds, "the slot is left alone");

    // Back, while the tab is still hidden: suspended then.
    player.pip(e, false, [16, 9]).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: e, on: false });
    assert_eq!(next_p(&mut app), ParentEvent::Suspended { embed: e });
    // Shown again: bound at the rect the parent last placed, and resumed.
    app.set_visible(e, true).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Resumed { embed: e });
    assert_eq!(w.fresco.binds().last().unwrap().3, Some([0.0, 0.0, 320.0, 180.0]));

    // In and back with the tab shown: straight into the slot, no Resumed.
    player.pip(e, true, [16, 9]).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: e, on: true });
    player.pip(e, false, [16, 9]).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: e, on: false });
    assert!(app.poll(Duration::from_millis(50)).unwrap().is_none());
    assert_eq!(w.fresco.binds().last().unwrap().3, Some([0.0, 0.0, 320.0, 180.0]));
}

#[test]
fn one_pip_at_a_time_and_none_outlives_its_embed() {
    let w = World::new();
    let mut app = w.parent(APP);
    let (a, mut pa) = bound_player(&w, &mut app, W_PLAYER);
    let (b, mut pb) = bound_player(&w, &mut app, W_PLAYER2);
    w.fresco.1.lock().unwrap().pressed.push(W_PLAYER);
    w.fresco.1.lock().unwrap().pressed.push(W_PLAYER2);
    pa.pip(a, true, [16, 9]).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: a, on: true });
    pb.pip(b, true, [16, 9]).unwrap();
    // The first goes back to its slot as the second takes its place.
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: a, on: false });
    assert_eq!(next_p(&mut app), ParentEvent::Pip { embed: b, on: true });
    assert_eq!(next_c(&mut pa), ChildEvent::Pip { embed: a, on: true });
    assert_eq!(next_c(&mut pa), ChildEvent::Pip { embed: a, on: false });
    assert_eq!(w.fresco.1.lock().unwrap().released, vec![W_PLAYER, W_PLAYER2]);
    assert_eq!(w.fresco.binds().last().unwrap().1, W_PLAYER, "a is back in its slot");
    // Closing the tab ends the PiP with it.
    app.detach(b).unwrap();
    assert!(matches!(next_c(&mut pb), ChildEvent::Pip { on: true, .. }));
    drop(pb);
}

#[test]
fn the_picker_is_scriniums_and_hands_back_one_regular_file() {
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsFd;
    let w = World::new();
    // Only Scrinium may provide it; with none, no picker.
    let mut app = w.parent(APP);
    assert_eq!(refused(app.request("picker", W_APP)), refusal::NO_PROVIDER);
    assert!(matches!(provider(w.connect(APP), "picker"), Err(Error::Refused(r)) if r == refusal::NOT_PROVIDER));
    let mut picker = provider(w.connect(SCRINIUM_UID), "picker").unwrap();

    let e = app.request("picker", W_APP).unwrap();
    assert_eq!(next_c(&mut picker), ChildEvent::Attach { embed: e, role: "picker".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
    // The picker hears whose persona it picks for (personas.md §4.1).
    assert_eq!(next_c(&mut picker), ChildEvent::ParentPersona { embed: e, persona: Some("work".into()) });
    let ask = limen::picker::ToChild::Open(limen::picker::Filter::file());
    app.send(e, ask.encode()).unwrap();
    assert!(matches!(next_c(&mut picker), ChildEvent::Message { .. }));

    #[cfg(any(target_os = "freebsd", target_os = "linux"))]
    {
        let staged = limen::fdpass::sealed_file("workbook", b"complete snapshot").unwrap();
        let save = limen::picker::ToChild::Open(limen::picker::Filter::save("Budget.osheet", "")).encode();
        assert_eq!(refused(app.send_fd(e, ask.encode(), staged.as_fd())), refusal::MALFORMED);
        app.send_fd(e, save, staged.as_fd()).unwrap();
        assert!(matches!(next_c(&mut picker), ChildEvent::MessageFd { .. }));
        let mut got = std::fs::File::from(picker.take_fd().unwrap());
        let mut bytes = Vec::new();
        got.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"complete snapshot");
        let commit = limen::picker::ToChild::Commit([42; 16]);
        app.send_fd(e, commit.encode(), staged.as_fd()).unwrap();
        assert!(matches!(next_c(&mut picker), ChildEvent::MessageFd { bytes, .. } if bytes == commit.encode()));
        assert!(picker.take_fd().is_some());
    }

    let path = std::env::temp_dir().join(format!("limen-pick-{}", std::process::id()));
    std::fs::write(&path, b"picked bytes").unwrap();
    let ro = std::fs::File::open(&path).unwrap();
    let file = limen::picker::ToParent::File { name: "notes.txt".into(), size: 12, write: false };
    // Not before the user could see it: the slot must be attached.
    assert_eq!(refused(picker.emit_fd(e, file.encode(), ro.as_fd())), refusal::NOT_ATTACHED);
    picker.ready(e, W_PICKER).unwrap();
    app.place(e, RECT).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: e });

    picker.emit_fd(e, file.encode(), ro.as_fd()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::MessageFd { embed: e, bytes: file.encode() });
    let mut got = std::fs::File::from(app.take_fd().expect("the picked file"));
    let mut s = String::new();
    got.read_to_string(&mut s).unwrap();
    assert_eq!(s, "picked bytes");
    got.rewind().unwrap();
    assert!(got.write_all(b"x").is_err(), "read-only stays read-only");

    // Read-write: the app may change it.
    let rw = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    picker.emit_fd(e, limen::picker::ToParent::File { name: "notes.txt".into(), size: 12, write: true }.encode(), rw.as_fd()).unwrap();
    assert!(matches!(next_p(&mut app), ParentEvent::MessageFd { .. }));
    let mut got = std::fs::File::from(app.take_fd().unwrap());
    got.write_all(b"changed").unwrap();
    drop(got);
    assert!(std::fs::read_to_string(&path).unwrap().starts_with("changed"));

    // Nothing but a regular file: not a directory, a socket or a pipe.
    let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
    assert_eq!(refused(picker.emit_fd(e, file.encode(), dir.as_fd())), refusal::BAD_FD);
    let (sock, _peer) = UnixStream::pair().unwrap();
    assert_eq!(refused(picker.emit_fd(e, file.encode(), sock.as_fd())), refusal::BAD_FD);
    let (r, _w) = std::io::pipe().unwrap();
    assert_eq!(refused(picker.emit_fd(e, file.encode(), r.as_fd())), refusal::BAD_FD);
    // A plain message carries none.
    picker.emit(e, limen::picker::ToParent::Done { files: 2 }.encode()).unwrap();
    assert!(matches!(next_p(&mut app), ParentEvent::Message { .. }));
    assert!(app.take_fd().is_none());

    // A viewer cannot return a file, and arbitrary picker messages cannot carry one.
    let v = app.request("media-player", W_APP).unwrap();
    let (_, mut viewer) = w.spawned();
    assert_eq!(refused(viewer.emit_fd(v, b"x".to_vec(), ro.as_fd())), refusal::MALFORMED);
    assert_eq!(refused(app.send_fd(e, b"x".to_vec(), ro.as_fd())), refusal::MALFORMED);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_parent_embeds_only_the_roles_its_manifest_names() {
    let w = World::new();
    // As limend's check would answer for a manifest with embeds = ["doc-viewer"].
    w.shared.set_embed_check(Box::new(|app: &str, role: &str| app == "org.example.mail" && role == "doc-viewer"));
    let mut app = w.parent(APP);
    assert_eq!(refused(app.request("map", W_APP)), refusal::NOT_EMBEDDABLE);
    assert_eq!(refused(app.request("picker", W_APP)), refusal::NOT_EMBEDDABLE);
    let mut other = w.parent(OTHER);
    assert_eq!(refused(other.request("doc-viewer", W_OTHER)), refusal::NOT_EMBEDDABLE);
    // The named role goes on to its provider.
    let e = app.request("doc-viewer", W_APP).unwrap();
    let (spawned, _child) = w.spawned();
    assert_eq!(e, spawned);
}

#[test]
fn a_share_target_is_only_ever_the_app_the_person_chose_in_the_parents_share_sheet() {
    use std::io::Read;
    use std::os::fd::AsFd;
    use limen::share_target::{Body, Format, Item};
    let w = World::new();
    w.shared.set_target_check(Box::new(|person: &str, app: &str| person == "ana" && app == "org.example.notes"));
    let mut app = w.parent(APP);
    let mut other = w.parent(OTHER);
    // No choice, no target: there is no default for this role.
    assert_eq!(refused(app.request("share-target", W_APP)), refusal::NO_PROVIDER);
    // Only the catalogue's share sheet provides `share`.
    assert!(matches!(provider(w.connect(APP), "share"), Err(Error::Refused(r)) if r == refusal::NOT_PROVIDER));
    let mut sheet = provider(w.connect(SHARE_UID), "share").unwrap();

    let s = app.request("share", W_APP).unwrap();
    assert_eq!(next_c(&mut sheet), ChildEvent::Attach { embed: s, role: "share".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
    let open = limen::share::ToChild::Open(limen::share::Open { activation: [5; 16], title: "Lake".into(), mimes: vec!["image/png".into(), "text/plain".into()] });
    app.send(s, open.encode()).unwrap();
    assert_eq!(next_c(&mut sheet), ChildEvent::Message { embed: s, bytes: open.encode() });
    sheet.ready(s, W_SHEET).unwrap();
    app.place(s, RECT).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: s });
    assert!(w.fresco.binds().iter().any(|b| b.1 == W_SHEET && b.2 == SlotKind::Sheet { w: 480, h: 400 }));

    // Only the sheet chooses, only for its own embed, only a real target.
    assert_eq!(refused(sheet.choose(s + 1000, "org.example.notes")), refusal::NOT_YOURS);
    assert_eq!(refused(sheet.choose(s, "org.example.evil")), refusal::NOT_A_TARGET);
    assert_eq!(refused(sheet.choose(s, "../org.example.notes")), refusal::NOT_A_TARGET);
    let mut pretender = Child::wrap(w.connect(APP));
    assert_eq!(refused(pretender.choose(s, "org.example.notes")), refusal::NOT_YOURS);

    sheet.choose(s, "org.example.notes").unwrap();
    sheet.emit(s, limen::share::ToParent::Chosen.encode()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed: s, bytes: limen::share::ToParent::Chosen.encode() });
    app.detach(s).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Detached { embed: s });
    // The choice is the app's: another app gets nothing from it.
    assert_eq!(refused(other.request("share-target", W_OTHER)), refusal::NO_PROVIDER);

    let t = app.request("share-target", W_APP).unwrap();
    let (te, mut notes) = w.spawned();
    assert_eq!(te, t);
    assert_eq!(next_c(&mut notes), ChildEvent::Attach { embed: t, role: "share-target".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
    // Once: a second request finds no choice.
    assert_eq!(refused(app.request("share-target", W_APP)), refusal::NO_PROVIDER);

    let item = Item { title: "Lake".into(), formats: vec![
        Format { mime: "image/png".into(), body: Body::Fd { size: 9 } },
        Format { mime: "text/plain".into(), body: Body::Inline(b"Lake".to_vec()) },
    ] };
    let path = std::env::temp_dir().join(format!("limen-share-{}", std::process::id()));
    std::fs::write(&path, b"png bytes").unwrap();
    let f = std::fs::File::open(&path).unwrap();
    app.send(t, limen::share_target::ToChild::Share(item.clone()).encode()).unwrap();
    app.send_fd(t, limen::share_target::ToChild::Part { index: 0 }.encode(), f.as_fd()).unwrap();
    let ChildEvent::Message { bytes, .. } = next_c(&mut notes) else { panic!() };
    assert_eq!(limen::share_target::ToChild::decode(&bytes), Some(limen::share_target::ToChild::Share(item)));
    let ChildEvent::MessageFd { bytes, .. } = next_c(&mut notes) else { panic!() };
    assert_eq!(limen::share_target::ToChild::decode(&bytes), Some(limen::share_target::ToChild::Part { index: 0 }));
    let mut got = String::new();
    std::fs::File::from(notes.take_fd().unwrap()).read_to_string(&mut got).unwrap();
    assert_eq!(got, "png bytes");
    // A writable descriptor is refused.
    let rw = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    assert_eq!(refused(app.send_fd(t, limen::share_target::ToChild::Part { index: 0 }.encode(), rw.as_fd())), refusal::BAD_FD);

    notes.ready(t, W_NOTES).unwrap();
    app.place(t, RECT).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: t });
    assert!(w.fresco.binds().iter().any(|b| b.1 == W_NOTES && b.2 == SlotKind::Sheet { w: 560, h: 440 }));
    notes.emit(t, limen::share_target::ToParent::Accepted.encode()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed: t, bytes: limen::share_target::ToParent::Accepted.encode() });
    let _ = std::fs::remove_file(&path);
}

#[test]
fn only_settings_opens_an_apps_own_pane_and_the_launcher_decides_the_persona() {
    let w = World::new();
    w.shared.set_pane_check(Box::new(|person: &str, app: &str| person == "ana" && app == "org.example.notes"));
    let mut curia = w.parent(CURIA_UID);
    let mut app = w.parent(APP);
    // Only Curia, and only through its own op.
    assert_eq!(refused(app.request_app_settings(W_APP, "org.example.notes", "personal")), refusal::NO_PANE);
    assert_eq!(refused(curia.request("app-settings", W_CURIA)), refusal::NO_PROVIDER);
    // Only an app with a pane, and only names limend can check.
    assert_eq!(refused(curia.request_app_settings(W_CURIA, "org.example.player", "personal")), refusal::NO_PANE);
    assert_eq!(refused(curia.request_app_settings(W_CURIA, "../org.example.notes", "personal")), refusal::NO_PANE);
    assert_eq!(refused(curia.request_app_settings(W_CURIA, "org.example.notes", "Work!")), refusal::NO_PANE);
    assert_eq!(refused(curia.request_app_settings(W_CURIA, "org.example.notes", "")), refusal::NO_PANE);

    // The pane goes to the launcher in the persona named: Personal, which
    // a share's launch would refuse, so it went the settings way.
    let e = curia.request_app_settings(W_CURIA, "org.example.notes", "personal").unwrap();
    let (spawned, mut pane) = w.spawned();
    assert_eq!(e, spawned);
    assert_eq!(next_c(&mut pane), ChildEvent::Attach { embed: e, role: "app-settings".into(), parent_app: catalogue::CURIA.into(), parent_uid: CURIA_UID });
    // One pane per Settings window.
    assert_eq!(refused(curia.request_app_settings(W_CURIA, "org.example.notes", "work")), refusal::TOO_MANY);
    pane.ready(e, W_NOTES).unwrap();
    curia.place(e, RECT).unwrap();
    assert_eq!(next_p(&mut curia), ParentEvent::Attached { embed: e });
    assert!(w.fresco.binds().iter().any(|b| b.0 == W_CURIA && b.1 == W_NOTES && b.2 == SlotKind::Plain));
    curia.detach(e).unwrap();
    assert_eq!(next_p(&mut curia), ParentEvent::Detached { embed: e });

    let e = curia.request_app_settings(W_CURIA, "org.example.notes", "work").unwrap();
    let (spawned, _pane) = w.spawned();
    assert_eq!(e, spawned);
    curia.detach(e).unwrap();
    assert_eq!(next_p(&mut curia), ParentEvent::Detached { embed: e });
    // A persona the launcher won't start it in: nothing starts.
    assert_eq!(refused(curia.request_app_settings(W_CURIA, "org.example.notes", "school")), refusal::LAUNCH_FAILED);
}

#[test]
fn a_plugin_is_any_app_the_parent_names_that_is_one_for_its_person() {
    let w = World::new();
    w.shared.set_plugin_check(Box::new(|person: &str, app: &str| person == "ana" && ["org.example.notes", "org.example.map"].contains(&app)));
    let mut app = w.parent(APP);
    // Never from a plain request, and only apps that are plugins, by names
    // limend can check.
    assert_eq!(refused(app.request("editor-extension", W_APP)), refusal::NO_PROVIDER);
    assert_eq!(refused(app.request_plugin(W_APP, "org.example.player")), refusal::NO_PLUGIN);
    assert_eq!(refused(app.request_plugin(W_APP, "../org.example.notes")), refusal::NO_PLUGIN);
    assert_eq!(refused(app.request_plugin(W_APP, "")), refusal::NO_PLUGIN);

    // Two plugins side by side, each started as itself.
    let a = app.request_plugin(W_APP, "org.example.notes").unwrap();
    let (spawned, mut notes) = w.spawned();
    assert_eq!(a, spawned);
    assert_eq!(next_c(&mut notes), ChildEvent::Attach { embed: a, role: "editor-extension".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
    let b = app.request_plugin(W_APP, "org.example.map").unwrap();
    let (spawned, _map) = w.spawned();
    assert_eq!(b, spawned);
    assert_ne!(a, b);
    // But one of each per window.
    assert_eq!(refused(app.request_plugin(W_APP, "org.example.notes")), refusal::TOO_MANY);

    // LSP travels without a panel; a panel is the plugin's window, bound
    // in a plain slot.
    let init = limen::editor_ext::ToChild::Init(limen::editor_ext::Init { languages: vec!["rust".into()], workspace: "hello".into(), read: false });
    app.send(a, init.encode()).unwrap();
    assert_eq!(next_c(&mut notes), ChildEvent::Message { embed: a, bytes: init.encode() });
    for part in limen::editor_ext::split(br#"{"jsonrpc":"2.0","id":1,"result":null}"#, false).unwrap() {
        notes.emit(a, part.clone()).unwrap();
        assert_eq!(next_p(&mut app), ParentEvent::Message { embed: a, bytes: part });
    }
    notes.ready(a, W_NOTES).unwrap();
    app.place(a, RECT).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: a });
    assert!(w.fresco.binds().iter().any(|x| x.0 == W_APP && x.1 == W_NOTES && x.2 == SlotKind::Plain));

    // Which plugins there are: only for a parent that may embed them.
    let info = limen::proto::PluginInfo { app: "org.example.notes".into(), name: "Notes".into(), languages: vec!["markdown".into()],
        commands: vec![], panel: None, workspace_read: false, network: vec![] };
    let listed = info.clone();
    w.shared.set_plugin_list(Box::new(move |person: &str| if person == "ana" { vec![listed.clone(); 100] } else { vec![] }));
    assert_eq!(app.list_plugins().unwrap(), vec![info; catalogue::MAX_PLUGINS], "at most MAX_PLUGINS");
    w.shared.set_embed_check(Box::new(|app: &str, role: &str| app == "org.example.other" && role == "editor-extension"));
    assert_eq!(refused(app.list_plugins()), refusal::NOT_EMBEDDABLE);
    let mut other = w.parent(OTHER);
    assert_eq!(other.list_plugins().unwrap().len(), catalogue::MAX_PLUGINS);
    w.shared.set_embed_check(Box::new(|_: &str, _: &str| true));

    // Once it is gone the same plugin can start again (Restart).
    app.detach(a).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Detached { embed: a });
    let c = app.request_plugin(W_APP, "org.example.notes").unwrap();
    assert_eq!(w.spawned().0, c);
}

#[test]
fn a_choice_in_another_persona_goes_to_the_launcher_which_decides() {
    let w = World::new();
    w.shared.set_target_check(Box::new(|person: &str, app: &str| person == "ana" && app == "org.example.notes"));
    let mut app = w.parent(APP);
    let mut sheet = provider(w.connect(SHARE_UID), "share").unwrap();
    let mut open = |app: &mut Parent, sheet: &mut Child| {
        let s = app.request("share", W_APP).unwrap();
        let _ = next_c(sheet);
        s
    };
    // Not a persona's name: refused by limend.
    let s = open(&mut app, &mut sheet);
    assert_eq!(refused(sheet.choose_in(s, "org.example.notes", "Work!")), refusal::NOT_A_TARGET);
    assert_eq!(refused(sheet.choose_in(s, "org.example.notes", "")), refusal::NOT_A_TARGET);
    // Into Work: the launcher (portcullisd) has the switch on.
    sheet.choose_in(s, "org.example.notes", "work").unwrap();
    app.detach(s).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Detached { embed: s });
    let t = app.request("share-target", W_APP).unwrap();
    let (te, _notes) = w.spawned();
    assert_eq!(te, t);
    // Into School: the launcher says the switch is off; nothing starts.
    let s = open(&mut app, &mut sheet);
    sheet.choose_in(s, "org.example.notes", "school").unwrap();
    app.detach(s).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Detached { embed: s });
    assert_eq!(refused(app.request("share-target", W_APP)), refusal::LAUNCH_FAILED);
}

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
#[test]
fn the_print_sheet_takes_a_sealed_pdf_hands_back_save_as_pdf_and_opens_once_per_window() {
    use std::io::Read;
    use std::os::fd::AsFd;
    use limen::print::{ToChild, ToParent};
    let w = World::new();
    let mut app = w.parent(APP);
    assert_eq!(refused(app.request("print", W_APP)), refusal::NO_PROVIDER);
    assert!(matches!(provider(w.connect(SCRINIUM_UID), "print"), Err(Error::Refused(r)) if r == refusal::NOT_PROVIDER));
    let mut sheet = provider(w.connect(PRELUM_UID), "print").unwrap();

    let e = app.request("print", W_APP).unwrap();
    assert_eq!(next_c(&mut sheet), ChildEvent::Attach { embed: e, role: "print".into(), parent_app: "org.example.mail".into(), parent_uid: APP });
    // One sheet per window at a time.
    assert_eq!(refused(app.request("print", W_APP)), refusal::TOO_MANY);

    // A document made in memory, sealed, counts as read-only.
    let pdf = limen::fdpass::sealed_file("doc.pdf", b"%PDF-1.7 made in memory").unwrap();
    let ask = ToChild::Print { title: "Q4".into(), page_size_hint: None, can_relayout: false };
    app.send_fd(e, ask.encode(), pdf.as_fd()).unwrap();
    assert_eq!(next_c(&mut sheet), ChildEvent::MessageFd { embed: e, bytes: ask.encode() });
    let mut got = std::fs::File::from(sheet.take_fd().expect("the document"));
    let mut b = String::new();
    got.read_to_string(&mut b).unwrap();
    assert_eq!(b, "%PDF-1.7 made in memory");
    // An unsealed read-write file is not.
    let path = std::env::temp_dir().join(format!("limen-print-{}", std::process::id()));
    std::fs::write(&path, b"%PDF").unwrap();
    let rw = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    assert_eq!(refused(app.send_fd(e, ask.encode(), rw.as_fd())), refusal::BAD_FD);

    // Save as PDF comes back read-only.
    sheet.ready(e, W_PRINT).unwrap();
    app.place(e, RECT).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: e });
    let ro = std::fs::File::open(&path).unwrap();
    let save = ToParent::SavePdf { name: "Q4.pdf".into() };
    sheet.emit_fd(e, save.encode(), ro.as_fd()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::MessageFd { embed: e, bytes: save.encode() });
    assert!(app.take_fd().is_some());
    let _ = std::fs::remove_file(&path);

    // Closed, it opens again; a window that keeps opening it is slowed.
    app.detach(e).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let e2 = app.request("print", W_APP).unwrap();
    app.detach(e2).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let e3 = app.request("print", W_APP).unwrap();
    app.detach(e3).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(refused(app.request("print", W_APP)), refusal::RATE, "the fourth in a minute, at once");
}

#[test]
fn vpn_page_requires_the_signed_navigator_in_the_vpn_persona() {
    let w = World::new();
    w.shared.set_persona_lookup(Box::new(|uid| (uid != 4101).then(|| "vpn".into())));
    w.shared.set_mail_sheet_provider(Box::new(|uid| matches!(uid, 4101 | 4112)));
    for uid in [4101, 4102, 4112] {
        assert_eq!(refused(w.parent(uid).request("vpn-sign-in-page", W_APP)), refusal::NOT_PROVIDER);
    }
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4102));
    assert!(w.parent(4102).request("vpn-sign-in-page", W_APP).is_ok());
}

#[test]
fn mail_sheet_requires_its_publisher_owner_and_persona() {
    let w = World::new();
    w.shared.set_persona_lookup(Box::new(|uid| matches!(uid, 4102 | 4112).then(|| "school".into())));
    assert_eq!(refused(provider(w.connect(4101), "mail-sign-in").map(|_| ())), refusal::NOT_PROVIDER);
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    let _sheet = provider(w.connect(4101), "mail-sign-in").unwrap();
    assert_eq!(refused(w.parent(4101).request("mail-sign-in-page", W_APP)), refusal::NOT_PROVIDER);
    assert_eq!(refused(w.parent(4111).request("mail-sign-in-page", W_APP)), refusal::NOT_PROVIDER);
    assert_eq!(refused(provider(w.connect(4102), "mail-sign-in").map(|_| ())), refusal::NOT_PROVIDER);
    assert!(w.parent(4111).request("mail-sign-in", W_APP).is_ok());
    assert_eq!(refused(w.parent(4112).request("mail-sign-in", W_APP)), refusal::NO_PROVIDER);
    assert_eq!(refused(w.parent(4113).request("mail-sign-in", W_APP)), refusal::NO_PROVIDER);
}

#[test]
fn island_fetch_channels_are_private_to_the_signed_navigator_request() {
    use std::io::Read;
    let w = World::new();
    let mut navigator = w.parent(4101);
    assert_eq!(refused(navigator.request_island(W_APP, 1)), refusal::NOT_PROVIDER);
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101 || uid == OTHER));
    assert_eq!(refused(w.parent(OTHER).request_island(W_APP, 1)), refusal::NOT_PROVIDER);
    assert_eq!(refused(w.parent(4102).request_island(W_APP, 1)), refusal::NOT_PROVIDER);

    assert_eq!(refused(w.parent(OTHER).request_page_island(W_APP, 1)), refusal::NOT_PROVIDER);
    let first = navigator.request_island(W_APP, 1).unwrap();
    let second = navigator.request_island(W_APP, 1).unwrap();
    // A whole page: the engine manifest's limit, not a part's.
    let page = navigator.request_page_island(W_APP, 1).unwrap();
    for (id, limit) in [(first, limen::web_island::PART_MEMORY_MB), (second, limen::web_island::PART_MEMORY_MB), (page, 0)] {
        assert_eq!(next_p(&mut navigator), ParentEvent::IslandFetch { embed: id });
        assert!(navigator.take_fd().is_none());
        assert!(navigator.take_island_fetch(id + 9).is_none());
        let mut stream = navigator.take_island_fetch(id).unwrap();
        // The fake worker wrote its marker before launch returned.
        stream.set_nonblocking(true).unwrap();
        let mut message = [0; 16];
        stream.read_exact(&mut message).unwrap();
        assert_eq!(u64::from_le_bytes(message[..8].try_into().unwrap()), id);
        assert_eq!(u64::from_le_bytes(message[8..].try_into().unwrap()), limit, "embed {id}");
        assert!(navigator.take_island_fetch(id).is_none());
        navigator.detach(id).unwrap();
    }
}

#[test]
fn delegated_navigator_ui_keeps_document_embeds_and_cannot_launch_an_engine() {
    use std::os::unix::net::UnixListener;

    let w = World::new();
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    let path = std::env::temp_dir().join(format!("limen-ui-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    let shared = w.shared.clone();
    std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let _ = serve(aqueduct::Connection::wrap(s).unwrap(), 4101, shared);
    });
    let socket = limen::client::navigator_ui_channel(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let mut ui = Parent::wrap(aqueduct::Connection::wrap(socket).unwrap());
    assert_eq!(refused(ui.request_island(W_APP, 1)), refusal::NOT_PROVIDER);
    assert_eq!(refused(ui.request_page_island(W_APP, 1)), refusal::NOT_PROVIDER);
    assert_eq!(refused(ui.request("web-island", W_APP)), refusal::NOT_PROVIDER);
    assert_eq!(refused(ui.request("navigator-inbox-page", W_APP)), refusal::NOT_PROVIDER);
    assert_eq!(w.shared.embeds(), 0);
    let doc = ui.request("doc-viewer", W_APP).unwrap();
    ui.detach(doc).unwrap();
    let broker = w.parent(4101).request_island(W_APP, 1).unwrap();
    assert_ne!(doc, broker);
}

#[test]
fn an_islands_parent_spends_a_press_in_the_island_once() {
    let w = World::new();
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    w.fresco.0.lock().unwrap().0.insert(W_APP, 4101);
    let mut app = w.parent(4101);
    let id = app.request_island(W_APP, 1).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::IslandFetch { embed: id });
    let _fetch = app.take_island_fetch(id).unwrap();
    let (_, mut child) = w.spawned();
    let _ = next_c(&mut child);
    w.fresco.1.lock().unwrap().pressed.push(W_VIEWER);
    assert_eq!(refused(app.spend_press(id)), refusal::NO_PRESS, "not bound yet");
    app.place(id, Some([0., 0., 640., 360.])).unwrap();
    child.ready(id, W_VIEWER).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: id });
    assert_eq!(refused(w.parent(OTHER).spend_press(id)), refusal::NOT_YOURS);
    assert_eq!(refused(app.spend_press(id + 1)), refusal::NOT_YOURS);
    app.spend_press(id).unwrap();
    assert_eq!(refused(app.spend_press(id)), refusal::NO_PRESS, "spent");
    // A press on the parent's own window is not the island's.
    w.fresco.1.lock().unwrap().pressed.push(W_APP);
    assert_eq!(refused(app.spend_press(id)), refusal::NO_PRESS);
    w.fresco.1.lock().unwrap().pressed.push(W_VIEWER);
    app.spend_press(id).unwrap();
}

#[test]
fn fetched_island_reports_follow_attachment_direction_and_lifecycle() {
    use limen::web_island::{EndReason, Report, MAX_FRAME};
    let w = World::new();
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    w.fresco.0.lock().unwrap().0.insert(W_APP, 4101);
    let mut app = w.parent(4101);
    let id = app.request_island(W_APP, 1).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::IslandFetch { embed: id });
    let _fetch = app.take_island_fetch(id).unwrap();
    let (_, mut child) = w.spawned();
    let _ = next_c(&mut child);
    let started = Report::Started { origin: "https://widget.example" }.encode().unwrap();
    let title = Report::Title("Counter").encode().unwrap();
    assert_eq!(refused(child.emit(id, started.clone())), refusal::NOT_ATTACHED);
    app.place(id, Some([0., 0., 640., 360.])).unwrap();
    child.ready(id, W_VIEWER).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Attached { embed: id });
    assert_eq!(refused(app.send(id, started.clone())), refusal::MALFORMED);
    assert_eq!(refused(child.emit(id, vec![0; MAX_FRAME + 1])), refusal::TOO_BIG);
    assert_eq!(refused(child.emit(id, title.clone())), refusal::MALFORMED);
    assert_eq!(refused(child.emit(id, b"frame".to_vec())), refusal::MALFORMED);
    let mut padded = started.clone();
    padded.push(0);
    assert_eq!(refused(child.emit(id, padded)), refusal::MALFORMED);
    child.emit(id, started.clone()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed: id, bytes: started.clone() });
    let seek = limen::web_island::ChapterSeek::from_link("#t=1.5").unwrap().encode();
    assert_eq!(refused(app.send(id, seek.clone())), refusal::MALFORMED);
    let ready = Report::MediaReady.encode().unwrap();
    child.emit(id, ready.clone()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed: id, bytes: ready });
    app.send(id, seek.clone()).unwrap();
    assert_eq!(next_c(&mut child), ChildEvent::Message { embed: id, bytes: seek });
    let ask = Report::BeforeUnload { query: 3 }.encode().unwrap();
    let reply = limen::web_island::UnloadAnswer { query: 3, leave: false }.encode();
    assert_eq!(refused(app.send(id, reply.clone())), refusal::MALFORMED);
    child.emit(id, ask.clone()).unwrap();
    assert_eq!(next_p(&mut app), ParentEvent::Message { embed: id, bytes: ask.clone() });
    assert_eq!(refused(child.emit(id, ask)), refusal::MALFORMED);
    assert_eq!(refused(app.send(id, limen::web_island::UnloadAnswer { query: 2, leave: true }.encode())), refusal::MALFORMED);
    app.send(id, reply.clone()).unwrap();
    assert_eq!(next_c(&mut child), ChildEvent::Message { embed: id, bytes: reply.clone() });
    assert_eq!(refused(app.send(id, reply)), refusal::MALFORMED);
    assert_eq!(refused(child.emit(id, Report::BeforeUnload { query: 3 }.encode().unwrap())), refusal::MALFORMED);
    for bytes in [title.clone(), Report::Resize { width: 640, height: 360 }.encode().unwrap(),
                  Report::Ended(EndReason::Closed).encode().unwrap()] {
        child.emit(id, bytes.clone()).unwrap();
        assert_eq!(next_p(&mut app), ParentEvent::Message { embed: id, bytes });
        assert_eq!(refused(child.emit(id, started.clone())), refusal::MALFORMED);
    }
    assert_eq!(refused(child.emit(id, title)), refusal::MALFORMED);
}

#[cfg(feature = "legacy-smime-integration")]
#[test]
fn legacy_sheet_accepts_one_selection_before_attach() {
    let w = World::new();
    const MAIL_UID: u32 = 4111;
    const W_MAIL: WindowToken = [25; 16];
    w.fresco.0.lock().unwrap().0.insert(W_MAIL, MAIL_UID);
    w.shared.set_system_uid(catalogue::ARCA, ARCA_UID);
    w.shared.set_smime_mail(Box::new(|uid| (uid == MAIL_UID).then(|| Some("work".into()))));
    let mut arca = provider(w.connect(ARCA_UID), "smime-legacy-read").unwrap();
    let mut mail = w.parent(MAIL_UID);
    let sheet = mail.request("smime-legacy-read", W_MAIL).unwrap();
    assert!(matches!(next_c(&mut arca), ChildEvent::Attach { embed, .. } if embed == sheet));
    assert_eq!(next_c(&mut arca), ChildEvent::ParentPersona { embed: sheet, persona: Some("work".into()) });
    assert_eq!(refused(mail.send(sheet, vec![0; 43])), refusal::MALFORMED);
    let selected = vec![1; 45];
    mail.send(sheet, selected.clone()).unwrap();
    assert_eq!(next_c(&mut arca), ChildEvent::Message { embed: sheet, bytes: selected });
    assert_eq!(refused(mail.send(sheet, vec![2; 45])), refusal::MALFORMED);
    mail.place(sheet, Some([0.0, 0.0, 620.0, 350.0])).unwrap();
    arca.ready(sheet, W_ARCA).unwrap();
    assert_eq!(next_p(&mut mail), ParentEvent::Attached { embed: sheet });
    assert_eq!(refused(mail.send(sheet, vec![3; 45])), refusal::MALFORMED);
}

#[test]
fn smime_key_picker_routes_a_single_read_only_file_to_arca() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;

    use limen::picker::{Filter, ToChild, ToParent};

    let w = World::new();
    const MAIL_UID: u32 = 4111;
    const W_MAIL: WindowToken = [25; 16];
    w.fresco.0.lock().unwrap().0.insert(W_MAIL, MAIL_UID);
    w.shared.set_system_uid(catalogue::ARCA, ARCA_UID);
    w.shared.set_smime_mail(Box::new(|uid| (uid == MAIL_UID).then(|| Some("work".into()))));
    assert!(provider(w.connect(OTHER), "smime-provision").is_err());
    let mut arca = provider(w.connect(ARCA_UID), "smime-provision").unwrap();
    let mut picker = provider(w.connect(SCRINIUM_UID), "picker").unwrap();
    let mut mail = w.parent(MAIL_UID);
    let mut arca_parent = w.parent(ARCA_UID);
    assert_eq!(
        refused(w.parent(4112).request("smime-provision", W_OTHER)),
        refusal::NOT_AN_APP
    );
    let sheet = mail.request("smime-provision", W_MAIL).unwrap();
    assert!(matches!(next_c(&mut arca), ChildEvent::Attach { embed, role, parent_uid, .. }
        if embed == sheet && role == "smime-provision" && parent_uid == MAIL_UID));
    assert_eq!(next_c(&mut arca), ChildEvent::ParentPersona { embed: sheet, persona: Some("work".into()) });
    assert_eq!(refused(arca_parent.request_smime_picker(sheet, W_ARCA)), refusal::NOT_ATTACHED);
    assert_eq!(refused(mail.request_smime_picker(sheet, W_ARCA)), refusal::NOT_PROVIDER);
    assert_eq!(refused(mail.send(sheet, b"forged persona".to_vec())), refusal::MALFORMED);
    mail.place(sheet, Some([0.0, 0.0, 680.0, 520.0])).unwrap();
    arca.ready(sheet, W_ARCA).unwrap();
    assert_eq!(next_p(&mut mail), ParentEvent::Attached { embed: sheet });

    assert_eq!(refused(arca_parent.request_smime_picker(sheet, W_OTHER)), refusal::NOT_ATTACHED);
    let pick = arca_parent.request_smime_picker(sheet, W_ARCA).unwrap();
    assert_eq!(refused(arca_parent.request_smime_picker(sheet, W_ARCA)), refusal::TOO_MANY);
    assert_eq!(next_c(&mut picker), ChildEvent::Attach {
        embed: pick, role: "picker".into(), parent_app: catalogue::ARCA.into(), parent_uid: ARCA_UID,
    });
    assert_eq!(next_c(&mut picker), ChildEvent::ParentPersona { embed: pick, persona: Some("work".into()) });
    let invalid = ToChild::Open(Filter { write: true, ..Filter::file() });
    assert_eq!(refused(arca_parent.send(pick, invalid.encode())), refusal::MALFORMED);
    let open = ToChild::Open(Filter::file()).encode();
    arca_parent.send(pick, open.clone()).unwrap();
    assert_eq!(next_c(&mut picker), ChildEvent::Message { embed: pick, bytes: open });
    arca_parent.place(pick, Some([0.0, 0.0, 640.0, 440.0])).unwrap();
    picker.ready(pick, W_PICKER).unwrap();
    assert_eq!(next_p(&mut arca_parent), ParentEvent::Attached { embed: pick });
    assert_eq!(w.fresco.binds().last().unwrap().2, SlotKind::SmimePicker);

    let path = std::env::temp_dir().join(format!("limen-smime-picker-{}", std::process::id()));
    let mut key = std::fs::OpenOptions::new().create_new(true).mode(0o600).read(true).write(true).open(&path).unwrap();
    let bytes = b"synthetic private-key fixture";
    key.write_all(bytes).unwrap();
    let ro = std::fs::File::open(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let picked = ToParent::File { name: "test.pk8".into(), size: bytes.len() as u64, write: false }.encode();
    assert_eq!(refused(picker.emit_fd(pick, picked.clone(), key.as_fd())), refusal::BAD_FD);
    let wrong_size = ToParent::File { name: "test.pk8".into(), size: 1, write: false }.encode();
    assert_eq!(refused(picker.emit_fd(pick, wrong_size, ro.as_fd())), refusal::BAD_FD);
    key.set_len(16 * 1024 + 1).unwrap();
    let oversized = ToParent::File { name: "oversized.pk8".into(), size: 16 * 1024 + 1, write: false }.encode();
    assert_eq!(refused(picker.emit_fd(pick, oversized, ro.as_fd())), refusal::BAD_FD);
    key.set_len(bytes.len() as u64).unwrap();
    assert_eq!(refused(arca.emit_fd(sheet, picked.clone(), ro.as_fd())), refusal::MALFORMED);
    picker.emit_fd(pick, picked.clone(), ro.as_fd()).unwrap();
    assert_eq!(next_p(&mut arca_parent), ParentEvent::MessageFd { embed: pick, bytes: picked.clone() });
    let mut arca_file = std::fs::File::from(arca_parent.take_fd().unwrap());
    let mut private = Vec::new();
    arca_file.read_to_end(&mut private).unwrap();
    assert_eq!(private, bytes);
    assert!(mail.poll(Duration::from_millis(40)).unwrap().is_none());
    assert!(mail.take_fd().is_none());
    assert_eq!(refused(picker.emit_fd(pick, picked.clone(), ro.as_fd())), refusal::MALFORMED);

    let done = ToParent::Done { files: 1 }.encode();
    picker.emit(pick, done.clone()).unwrap();
    assert_eq!(next_p(&mut arca_parent), ParentEvent::Message { embed: pick, bytes: done });
    assert_eq!(refused(arca.emit(sheet, bytes.to_vec())), refusal::MALFORMED);
    let outcome = limen::smime_provision::Outcome::Provisioned { fingerprint: [6; 32] }.encode();
    arca.emit(sheet, outcome.clone()).unwrap();
    assert_eq!(next_p(&mut mail), ParentEvent::Message { embed: sheet, bytes: outcome });
    mail.set_visible(sheet, false).unwrap();
    assert_eq!(next_p(&mut arca_parent), ParentEvent::Detached { embed: pick });
    assert_eq!(next_c(&mut picker), ChildEvent::Detaching { embed: pick });
    assert_eq!(refused(picker.emit_fd(pick, picked, ro.as_fd())), refusal::NOT_YOURS);
    assert_eq!(refused(arca_parent.request_smime_picker(sheet, W_ARCA)), refusal::NOT_ATTACHED);
    assert!(mail.take_fd().is_none());
}

#[test]
fn a_navigators_next_island_takes_its_warmed_standby_engine() {
    use std::io::{Read, Write};
    let w = World::new();
    w.standby.store(true, std::sync::atomic::Ordering::SeqCst);
    w.shared.set_standby_delay(Duration::ZERO);
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    let mut navigator = w.parent(4101);

    // The first island starts cold, and a standby is started behind it.
    let first = navigator.request_island(W_APP, 1).unwrap();
    let (instance, mut cold) = w.spawned();
    assert_eq!(instance, first);
    assert!(matches!(next_c(&mut cold), ChildEvent::Attach { role, .. } if role == "web-island"));
    let (standby, standby_channel) = w.launched.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_ne!(standby, first);
    eventually("the standby is registered", || w.shared.standby_ready(4101));
    // Before a press the standby has nothing: no fetch socket, no frescod
    // connection, no event.
    standby_channel.set_nonblocking(true).unwrap();
    let mut probe = [0u8; 1];
    assert!(matches!((&standby_channel).read(&mut probe), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    standby_channel.set_nonblocking(false).unwrap();

    // A whole page never takes it: the standby has a part's memory limit.
    let page = navigator.request_page_island(W_APP, 1).unwrap();
    assert!(![first, standby].contains(&page));
    let (instance, _page_child) = w.spawned();
    assert_eq!(instance, page);
    assert!(w.shared.standby_ready(4101), "the standby still waits");
    assert_eq!(next_p(&mut navigator), ParentEvent::IslandFetch { embed: first });
    assert_eq!(next_p(&mut navigator), ParentEvent::IslandFetch { embed: page });
    navigator.detach(page).unwrap();
    assert_eq!(next_p(&mut navigator), ParentEvent::Detached { embed: page });

    // The second island is the standby: same embed id, no new launch.
    let second = navigator.request_island(W_APP, 1).unwrap();
    assert_eq!(second, standby);
    // The engine's side: the fetch socket first, then frescod, then Attach.
    let mut fds = portcullis_ipc::recv_fds(&standby_channel, 1).unwrap();
    let mut fetch = UnixStream::from(fds.pop().unwrap());
    let (mut warm, _fresco) = Child::embedded_on(standby_channel).unwrap();
    assert!(matches!(next_c(&mut warm), ChildEvent::Attach { embed, role, .. } if embed == second && role == "web-island"));
    // The fetch socket reaches the navigator as this island's.
    assert_eq!(next_p(&mut navigator), ParentEvent::IslandFetch { embed: second });
    let mut broker = navigator.take_island_fetch(second).unwrap();
    fetch.write_all(b"warm").unwrap();
    let mut got = [0u8; 4];
    broker.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"warm");

    // And the next standby is on its way.
    let (third, _) = w.launched.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(![first, second].contains(&third));
    navigator.detach(second).unwrap();
    eventually("the warm island's jail ends", || w.ended.lock().unwrap().contains(&second));
}

#[test]
fn a_dead_standby_falls_back_to_a_cold_start() {
    let w = World::new();
    w.standby.store(true, std::sync::atomic::Ordering::SeqCst);
    w.shared.set_standby_delay(Duration::ZERO);
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    let mut navigator = w.parent(4101);
    let first = navigator.request_island(W_APP, 1).unwrap();
    let (_, _cold) = w.spawned();
    // The standby's engine dies before any press.
    let (standby, channel) = w.launched.recv_timeout(Duration::from_secs(2)).unwrap();
    eventually("the standby is registered", || w.shared.standby_ready(4101));
    drop(channel);
    let second = navigator.request_island(W_APP, 1).unwrap();
    assert!(![first, standby].contains(&second), "a fresh cold island");
    let (instance, mut child) = w.spawned();
    assert_eq!(instance, second);
    assert!(matches!(next_c(&mut child), ChildEvent::Attach { .. }));
    eventually("the dead standby is ended", || w.ended.lock().unwrap().contains(&standby));
}


#[test]
fn the_next_standby_waits_until_the_island_has_started() {
    let w = World::new();
    w.standby.store(true, std::sync::atomic::Ordering::SeqCst);
    w.shared.set_standby_delay(Duration::from_millis(400));
    w.shared.set_mail_sheet_provider(Box::new(|uid| uid == 4101));
    let mut navigator = w.parent(4101);
    let _ = navigator.request_island(W_APP, 1).unwrap();
    let (_, _cold) = w.spawned();
    // Nothing competes with the island's own start...
    assert!(w.launched.recv_timeout(Duration::from_millis(200)).is_err());
    // ...and then the standby comes.
    assert!(w.launched.recv_timeout(Duration::from_secs(2)).is_ok());
    eventually("the standby is registered", || w.shared.standby_ready(4101));
}
