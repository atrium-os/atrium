//! `pergola::share::Sharing` against limend in process (fake frescod and
//! portcullisd), a stand-in share sheet and a stand-in target.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use limen::catalogue::{self, SlotKind};
use limen::client::Child;
use limen::daemon::{serve, Compositor, Launcher, Shared};
use limen::proto::{ChildEvent, EmbedId, WindowToken};
use limen::share_target::{Body, Format, Incoming, Item};
use pergola::embed::Embeds;
use pergola::share::{Outcome, Sharing};

const APP: u32 = 1001;
const SHEET: u32 = 65045;
const W_APP: WindowToken = [1; 16];
const SLOTS: (u64, u64) = (1, 2);

#[derive(Clone, Default)]
struct Fresco(Arc<Mutex<Vec<SlotKind>>>);
impl Compositor for Fresco {
    fn bind(&mut self, _p: WindowToken, _c: WindowToken, k: SlotKind, _r: Option<[f32; 4]>, _z: u8) -> Result<(u32, u32), u8> {
        self.0.lock().unwrap().push(k);
        Err(2)
    }
    fn embed_connection(&mut self, _owner: u32) -> Result<UnixStream, u8> {
        let (a, b) = UnixStream::pair().unwrap();
        std::mem::forget(b);
        Ok(a)
    }
}

struct Launch(mpsc::Sender<(String, UnixStream)>);
impl Launcher for Launch {
    fn launch(&mut self, app: &str, _e: EmbedId, ch: UnixStream) -> Result<u32, String> {
        self.0.send((app.into(), ch)).unwrap();
        Ok(2000)
    }
    fn end(&mut self, _e: EmbedId) {}
}

fn connect(shared: &Arc<Shared>, uid: u32) -> aqueduct::Connection {
    let (a, b) = UnixStream::pair().unwrap();
    let sh = shared.clone();
    std::thread::spawn(move || { let _ = serve(aqueduct::Connection::wrap(b).unwrap(), uid, sh); });
    aqueduct::Connection::wrap(a).unwrap()
}

fn world() -> (Arc<Shared>, mpsc::Receiver<(String, UnixStream)>) {
    let (tx, rx) = mpsc::channel();
    let resolve = Box::new(|uid: u32| (uid == APP).then(|| ("ana".to_string(), "org.example.notes".to_string())));
    let shared = Shared::new(Box::new(Fresco::default()), Box::new(Launch(tx)), resolve, Box::new(|_: &str, _: &str| None));
    shared.set_system_uid(catalogue::SHARE, SHEET);
    (shared, rx)
}

fn next_c(c: &mut Child) -> ChildEvent { c.poll(Duration::from_secs(2)).unwrap().expect("a child event") }

/// Poll the app's embeds into `sharing` until it ends.
fn run(embeds: &std::rc::Rc<std::cell::RefCell<Embeds>>, s: &mut Sharing, until_step: impl Fn(&Sharing) -> bool) -> Option<Outcome> {
    for _ in 0..100 {
        if until_step(s) {
            return None;
        }
        let ev = embeds.borrow_mut().poll(Duration::from_millis(20)).unwrap();
        if let Some(ev) = ev {
            if let Some(o) = s.event(embeds, &ev) {
                return Some(o);
            }
        }
    }
    panic!("the share never moved on")
}

fn photo(path: &std::path::Path) -> (Item, Vec<(u8, OwnedFd)>) {
    std::fs::write(path, b"png!").unwrap();
    let item = Item { title: "Lake".into(), formats: vec![
        Format { mime: "image/png".into(), body: Body::Fd { size: 4 } },
        Format { mime: "text/plain".into(), body: Body::Inline(b"Lake".to_vec()) },
    ] };
    (item, vec![(0, OwnedFd::from(std::fs::File::open(path).unwrap()))])
}

#[test]
fn a_share_goes_through_the_sheet_to_the_chosen_app() {
    let (shared, launched) = world();
    let mut sheet = Child::wrap(connect(&shared, SHEET));
    sheet.provide_role("share").unwrap();
    let embeds = Embeds::wrap(limen::client::Parent::wrap(connect(&shared, APP)));
    let path = std::env::temp_dir().join(format!("pergola-share-{}", std::process::id()));
    let (item, files) = photo(&path);

    let mut s = Sharing::begin(&embeds, W_APP, [9; 16], item.clone(), files, SLOTS).unwrap();
    assert_eq!(s.slot(), Some(SLOTS.0));
    // The sheet gets the press, the title and the types, not the content.
    let ChildEvent::Attach { embed, role, .. } = next_c(&mut sheet) else { panic!() };
    assert_eq!(role, "share");
    let ChildEvent::Message { bytes, .. } = next_c(&mut sheet) else { panic!() };
    let Some(limen::share::ToChild::Open(o)) = limen::share::ToChild::decode(&bytes) else { panic!() };
    assert_eq!((o.activation, o.title.as_str(), o.mimes.clone()), ([9; 16], "Lake", vec!["image/png".to_string(), "text/plain".to_string()]));
    sheet.choose(embed, "org.example.scraps").unwrap();
    sheet.emit(embed, limen::share::ToParent::Chosen.encode()).unwrap();

    assert_eq!(run(&embeds, &mut s, |s| s.slot() == Some(SLOTS.1)), None);
    let (app, ch) = launched.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(app, "org.example.scraps");
    let (mut target, _fresco) = Child::embedded_on(ch).unwrap();
    let ChildEvent::Attach { embed: t, .. } = next_c(&mut target) else { panic!() };
    let mut inc = Incoming::default();
    let ChildEvent::Message { bytes, .. } = next_c(&mut target) else { panic!() };
    assert!(inc.message(&bytes));
    let ChildEvent::MessageFd { bytes, .. } = next_c(&mut target) else { panic!() };
    assert!(inc.message_fd(&bytes, target.take_fd().unwrap()));
    let got = inc.take().expect("the whole item");
    assert_eq!(got.item, item);
    assert_eq!(got.bytes(0, 100).unwrap(), b"png!");
    target.emit(t, limen::share_target::ToParent::Accepted.encode()).unwrap();
    assert_eq!(run(&embeds, &mut s, |_| false), Some(Outcome::Shared));
    assert_eq!(s.slot(), None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_cancelled_or_failed_sheet_ends_the_share_and_a_bad_item_never_starts() {
    let (shared, launched) = world();
    let mut sheet = Child::wrap(connect(&shared, SHEET));
    sheet.provide_role("share").unwrap();
    let embeds = Embeds::wrap(limen::client::Parent::wrap(connect(&shared, APP)));
    let path = std::env::temp_dir().join(format!("pergola-share-c-{}", std::process::id()));
    let (item, files) = photo(&path);
    // A picture announced by descriptor needs its file.
    assert!(Sharing::begin(&embeds, W_APP, [9; 16], item.clone(), vec![], SLOTS).is_err());
    let mut bad = Item::text("x", "y");
    bad.formats[0].mime = "text/*".into();
    assert!(Sharing::begin(&embeds, W_APP, [9; 16], bad, vec![], SLOTS).is_err());

    let mut s = Sharing::begin(&embeds, W_APP, [9; 16], item, files, SLOTS).unwrap();
    let ChildEvent::Attach { embed, .. } = next_c(&mut sheet) else { panic!() };
    let _ = next_c(&mut sheet);
    sheet.emit(embed, limen::share::ToParent::Cancelled.encode()).unwrap();
    assert_eq!(run(&embeds, &mut s, |_| false), Some(Outcome::NotShared));

    let mut s = Sharing::begin(&embeds, W_APP, [9; 16], Item::text("x", "y"), vec![], SLOTS).unwrap();
    let embed = loop {
        if let ChildEvent::Attach { embed, .. } = next_c(&mut sheet) { break embed }
    };
    sheet.emit(embed, limen::share::ToParent::Failed { why: "no press".into() }.encode()).unwrap();
    assert_eq!(run(&embeds, &mut s, |_| false), Some(Outcome::Failed("no press".into())));
    assert!(launched.try_recv().is_err(), "nothing was started");
    let _ = std::fs::remove_file(&path);
}
