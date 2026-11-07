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
const W_APP: WindowToken = [1; 16];
const W_OTHER: WindowToken = [2; 16];
const W_ARCA: WindowToken = [3; 16];
const W_VIEWER: WindowToken = [4; 16];

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
struct Launch(mpsc::Sender<(EmbedId, UnixStream)>, Arc<Mutex<Vec<EmbedId>>>);
impl Launcher for Launch {
    fn launch(&mut self, app: &str, embed: EmbedId, ch: UnixStream) -> Result<u32, String> {
        assert!(app == "org.example.viewer" || app == "org.example.player", "{app}");
        self.0.send((embed, ch)).unwrap();
        Ok(if app == "org.example.player" { PLAYER_UID } else { VIEWER_UID })
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
        let defaults = Box::new(|_: &str, role: &str| match role {
            "doc-viewer" => Some("org.example.viewer".to_string()),
            "media-player" => Some("org.example.player".to_string()),
            _ => None,
        });
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
    assert_eq!(tickets.0.lock().unwrap().1, vec![[PLAYER_UID as u8; 16]], "cancelled when the embed ends");

    // A parent choragusd refuses (no audio): the player gets no ticket.
    let mut other = w.parent(OTHER);
    let e = other.request("media-player", W_OTHER).unwrap();
    let (_, mut player) = w.spawned();
    assert!(matches!(next_c(&mut player), ChildEvent::Attach { .. }));
    other.detach(e).unwrap();
    assert_eq!(next_c(&mut player), ChildEvent::Detaching { embed: e }, "no AudioTicket in between");
    // Roles that don't play sound never ask.
    let _ = app.request("doc-viewer", W_APP).unwrap();
    let _ = w.spawned();
    assert_eq!(tickets.0.lock().unwrap().0.len(), 2);
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
    let v = app.request("doc-viewer", W_APP).unwrap();
    let _viewer = w.spawned();
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
