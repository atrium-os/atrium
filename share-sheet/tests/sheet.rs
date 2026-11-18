//! The share sheet against limend (in process, with fake frescod and
//! portcullisd) and a stand-in for frescod's activation table.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use fresco_protocol::activation::kind;
use limen::catalogue::{self, SlotKind};
use limen::client::{Child, Error, Parent};
use limen::daemon::{serve, Compositor, Launcher, Shared};
use limen::manifests::Installed;
use limen::proto::{refusal, EmbedId, ParentEvent, WindowToken};
use limen::share::{Open, ToChild, ToParent};
use share_sheet::{Event, Sheet};
use tabula::gate::Fake;

const MAIL: u32 = 1001;
const PHOTOS: u32 = 1002;
const BOB_APP: u32 = 1003;
const SHARE_UID: u32 = 65045;
const W_MAIL: WindowToken = [1; 16];
const W_BOB: WindowToken = [3; 16];

#[derive(Clone, Default)]
struct Fresco(Arc<Mutex<HashMap<WindowToken, u32>>>);
impl Compositor for Fresco {
    fn bind(&mut self, p: WindowToken, c: WindowToken, _k: SlotKind, _r: Option<[f32; 4]>, _z: u8) -> Result<(u32, u32), u8> {
        let g = self.0.lock().unwrap();
        match (g.get(&p), g.get(&c)) {
            (Some(&a), Some(&b)) => Ok((a, b)),
            _ => Err(2),
        }
    }
    fn embed_connection(&mut self, _owner: u32) -> Result<UnixStream, u8> {
        let (a, b) = UnixStream::pair().unwrap();
        std::mem::forget(b);
        Ok(a)
    }
}

/// portcullisd: says which app it was asked to start.
struct Launch(mpsc::Sender<(String, UnixStream)>);
impl Launcher for Launch {
    fn launch(&mut self, app: &str, _embed: EmbedId, ch: UnixStream) -> Result<u32, String> {
        self.0.send((app.to_string(), ch)).unwrap();
        Ok(2000)
    }
    fn end(&mut self, _embed: EmbedId) {}
}

fn app(dir: &Path, id: &str, extra: &str) {
    std::fs::create_dir_all(dir.join(id)).unwrap();
    std::fs::write(dir.join(id).join("atrium.toml"), format!("[app]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1\"\nentry = \"bin/x\"\n{extra}")).unwrap();
}

struct World {
    shared: Arc<Shared>,
    launched: mpsc::Receiver<(String, UnixStream)>,
    frescod: Fake,
    sheet: Sheet,
    dir: std::path::PathBuf,
}

impl World {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("share-sheet-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = "\n[roles]\nimplements = [\"share-target\"]\n";
        app(&dir, "org.example.mail", target);
        app(&dir, "org.example.notes", target);
        app(&dir, "org.example.photos", "\n[roles]\nimplements = [\"share-target\"]\n\n[roles.share-target]\nmime = [\"image/*\"]\n");
        app(&dir, "dev.ana.scratch.org.example.mine", target);
        app(&dir, "dev.bob.scratch.org.example.his", target);
        app(&dir, "org.example.maps", "\n[roles]\nimplements = [\"map\"]\n");
        let fresco = Fresco::default();
        {
            let mut g = fresco.0.lock().unwrap();
            g.insert(W_MAIL, MAIL);
            g.insert(W_BOB, BOB_APP);
        }
        let (tx, rx) = mpsc::channel();
        let resolve = Box::new(|uid: u32| match uid {
            MAIL => Some(("ana".into(), "org.example.mail".into())),
            PHOTOS => Some(("ana".into(), "org.example.photos".into())),
            BOB_APP => Some(("bob".into(), "org.example.mail".into())),
            _ => None,
        });
        let shared = Shared::new(Box::new(fresco), Box::new(Launch(tx)), resolve, Box::new(|_: &str, _: &str| None));
        shared.set_system_uid(catalogue::SHARE, SHARE_UID);
        let installed = Installed::at(&dir);
        let i2 = installed.clone();
        shared.set_target_check(Box::new(move |person: &str, app: &str| i2.may_target(person, app)));
        let mut child = Child::wrap(connect(&shared, SHARE_UID));
        child.provide_role("share").unwrap();
        let person = Box::new(|uid: u32| match uid {
            MAIL | PHOTOS => Some("ana".to_string()),
            BOB_APP => Some("bob".to_string()),
            _ => None,
        });
        let frescod = Fake::default();
        let sheet = Sheet::new(child, Box::new(frescod.clone()), installed, person);
        World { shared, launched: rx, frescod, sheet, dir }
    }

    fn connect(&self, uid: u32) -> aqueduct::Connection { connect(&self.shared, uid) }

    /// Serve until the sheet has something for the windows.
    fn event(&mut self) -> Option<Event> {
        for _ in 0..50 {
            if let Some(e) = self.sheet.poll(Duration::from_millis(20)).unwrap() {
                return Some(e);
            }
        }
        None
    }

    /// The app opens a sheet with `press` and the sheet serves the open.
    fn open(&mut self, parent: &mut Parent, window: WindowToken, press: [u8; 16], mimes: &[&str]) -> (EmbedId, Option<Event>) {
        let e = parent.request("share", window).unwrap();
        let open = Open { activation: press, title: "Lake at dawn".into(), mimes: mimes.iter().map(|m| m.to_string()).collect() };
        parent.send(e, ToChild::Open(open).encode()).unwrap();
        (e, self.event())
    }
}

impl Drop for World {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); }
}

/// A connection to limend as `uid`.
fn connect(shared: &Arc<Shared>, uid: u32) -> aqueduct::Connection {
    let (a, b) = UnixStream::pair().unwrap();
    let sh = shared.clone();
    std::thread::spawn(move || { let _ = serve(aqueduct::Connection::wrap(b).unwrap(), uid, sh); });
    aqueduct::Connection::wrap(a).unwrap()
}

fn sheet_says(p: &mut Parent) -> ToParent {
    loop {
        match p.poll(Duration::from_secs(2)).unwrap().expect("a parent event") {
            ParentEvent::Message { bytes, .. } => return ToParent::decode(&bytes).expect("a sheet message"),
            _ => continue,
        }
    }
}

fn targets(ev: Option<Event>) -> Vec<String> {
    match ev {
        Some(Event::Show { targets, .. }) => targets.into_iter().map(|t| t.app).collect(),
        other => panic!("expected the sheet shown, got {other:?}"),
    }
}

#[test]
fn a_sheet_needs_the_apps_own_fresh_press() {
    let mut w = World::new("press");
    let mut mail = Parent::wrap(w.connect(MAIL));
    // No press at all.
    let (_, ev) = w.open(&mut mail, W_MAIL, [0; 16], &["text/plain"]);
    assert_eq!(ev, None);
    assert!(matches!(sheet_says(&mut mail), ToParent::Failed { .. }));
    // Another app's press.
    let theirs = w.frescod.mint(PHOTOS, kind::BUTTON);
    let (_, ev) = w.open(&mut mail, W_MAIL, theirs, &["text/plain"]);
    assert_eq!(ev, None);
    assert!(matches!(sheet_says(&mut mail), ToParent::Failed { .. }));
    // A key press isn't a share.
    let key = w.frescod.mint(MAIL, kind::KEY);
    let (_, ev) = w.open(&mut mail, W_MAIL, key, &["text/plain"]);
    assert_eq!(ev, None);
    assert!(matches!(sheet_says(&mut mail), ToParent::Failed { .. }));
    // Its own click: shown. And spent: the same press opens nothing again.
    let click = w.frescod.mint(MAIL, kind::BUTTON);
    let (_, ev) = w.open(&mut mail, W_MAIL, click, &["text/plain"]);
    assert!(matches!(ev, Some(Event::Show { .. })));
    let (_, ev) = w.open(&mut mail, W_MAIL, click, &["text/plain"]);
    assert_eq!(ev, None);
}

#[test]
fn the_sheet_lists_targets_for_the_types_and_the_person_and_never_the_app_itself() {
    let mut w = World::new("list");
    let mut mail = Parent::wrap(w.connect(MAIL));
    let p = w.frescod.mint(MAIL, kind::BUTTON);
    let (_, ev) = w.open(&mut mail, W_MAIL, p, &["text/plain"]);
    // Not Mail itself, not Photos (pictures only), not bob's dev build, not Maps.
    assert_eq!(targets(ev), ["dev.ana.scratch.org.example.mine", "org.example.notes"]);
    let p = w.frescod.mint(MAIL, kind::BUTTON);
    let (_, ev) = w.open(&mut mail, W_MAIL, p, &["image/png", "text/plain"]);
    assert_eq!(targets(ev), ["dev.ana.scratch.org.example.mine", "org.example.notes", "org.example.photos"]);
    let mut bob = Parent::wrap(w.connect(BOB_APP));
    let p = w.frescod.mint(BOB_APP, kind::BUTTON);
    let (_, ev) = w.open(&mut bob, W_BOB, p, &["text/plain"]);
    assert_eq!(targets(ev), ["dev.bob.scratch.org.example.his", "org.example.notes"]);
}

#[test]
fn the_person_chooses_and_only_that_app_gets_the_share() {
    let mut w = World::new("choose");
    let mut mail = Parent::wrap(w.connect(MAIL));
    let p = w.frescod.mint(MAIL, kind::BUTTON);
    let (e, ev) = w.open(&mut mail, W_MAIL, p, &["text/plain"]);
    targets(ev);
    // Only what was shown.
    assert!(w.sheet.chose(e, "org.example.photos").is_err());
    assert!(w.sheet.chose(e, "org.example.evil").is_err());
    w.sheet.chose(e, "org.example.notes").unwrap();
    assert_eq!(sheet_says(&mut mail), ToParent::Chosen);
    // Once.
    assert!(w.sheet.chose(e, "dev.ana.scratch.org.example.mine").is_err());
    mail.detach(e).unwrap();
    assert!(matches!(w.event(), Some(Event::Close { embed }) if embed == e));
    let t = mail.request("share-target", W_MAIL).unwrap();
    let (started, _ch) = w.launched.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(started, "org.example.notes");
    let _ = t;
    // The choice was spent.
    assert!(matches!(mail.request("share-target", W_MAIL), Err(Error::Refused(r)) if r == refusal::NO_PROVIDER));
}

#[test]
fn closing_the_sheet_cancels_and_chooses_nothing() {
    let mut w = World::new("cancel");
    let mut mail = Parent::wrap(w.connect(MAIL));
    let p = w.frescod.mint(MAIL, kind::BUTTON);
    let (e, ev) = w.open(&mut mail, W_MAIL, p, &["text/plain"]);
    targets(ev);
    w.sheet.cancelled(e);
    assert_eq!(sheet_says(&mut mail), ToParent::Cancelled);
    assert!(w.sheet.chose(e, "org.example.notes").is_err(), "after a cancel");
    assert!(matches!(mail.request("share-target", W_MAIL), Err(Error::Refused(r)) if r == refusal::NO_PROVIDER));
}
