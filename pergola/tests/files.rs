//! `pergola::files::show` against limend in process (fake frescod and
//! portcullisd) and a stand-in Files.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use limen::catalogue::{self, SlotKind};
use limen::client::Child;
use limen::daemon::{serve, Compositor, Launcher, Shared};
use limen::picker::{Folder, ToChild, ToParent};
use limen::proto::{ChildEvent, EmbedId, WindowToken};
use pergola::embed::Embeds;

const APP: u32 = 1001;
const FILES: u32 = 65046;
const W_APP: WindowToken = [1; 16];
const SLOT: u64 = 7;

struct Fresco;
impl Compositor for Fresco {
    fn bind(&mut self, _p: WindowToken, _c: WindowToken, _k: SlotKind, _r: Option<[f32; 4]>, _z: u8) -> Result<(u32, u32), u8> { Err(2) }
    fn embed_connection(&mut self, _owner: u32) -> Result<UnixStream, u8> {
        let (a, b) = UnixStream::pair().unwrap();
        std::mem::forget(b);
        Ok(a)
    }
}

struct Launch;
impl Launcher for Launch {
    fn launch(&mut self, app: &str, _e: EmbedId, _ch: UnixStream) -> Result<u32, String> { Err(format!("nothing starts {app} here")) }
    fn end(&mut self, _e: EmbedId) {}
}

fn connect(shared: &Arc<Shared>, uid: u32) -> aqueduct::Connection {
    let (a, b) = UnixStream::pair().unwrap();
    let sh = shared.clone();
    std::thread::spawn(move || { let _ = serve(aqueduct::Connection::wrap(b).unwrap(), uid, sh); });
    aqueduct::Connection::wrap(a).unwrap()
}

fn next_c(c: &mut Child) -> ChildEvent { c.poll(Duration::from_secs(2)).unwrap().expect("a child event") }

fn answer(embeds: &std::rc::Rc<std::cell::RefCell<Embeds>>, ask: EmbedId) -> Result<(), String> {
    for _ in 0..100 {
        let ev = embeds.borrow_mut().poll(Duration::from_millis(20)).unwrap();
        if let Some(ev) = ev {
            if let Some(a) = pergola::files::shown(embeds, ask, &ev) { return a; }
        }
    }
    panic!("Files never answered")
}

#[test]
fn files_is_asked_to_show_a_name_in_a_folder_and_says_only_whether_it_took_it() {
    let resolve = Box::new(|uid: u32| (uid == APP).then(|| ("ana".to_string(), "org.example.notes".to_string())));
    let shared = Shared::new(Box::new(Fresco), Box::new(Launch), resolve, Box::new(|_: &str, _: &str| None));
    shared.set_system_uid(catalogue::SCRINIUM, FILES);
    let mut files = Child::wrap(connect(&shared, FILES));
    files.provide_role("picker").unwrap();
    let embeds = Embeds::wrap(limen::client::Parent::wrap(connect(&shared, APP)));

    let ask = pergola::files::show(&embeds, W_APP, SLOT, Folder::Downloads, "plan (1).pdf", [5; 16]).unwrap();
    let ChildEvent::Attach { embed, role, .. } = next_c(&mut files) else { panic!() };
    assert_eq!((embed, role.as_str()), (ask, "picker"));
    let ev = loop {
        match next_c(&mut files) {
            ChildEvent::ParentPersona { .. } => continue,
            ev => break ev,
        }
    };
    let ChildEvent::Message { bytes, .. } = ev else { panic!("{ev:?}") };
    assert_eq!(ToChild::decode(&bytes), Some(ToChild::Show { folder: Folder::Downloads, name: "plan (1).pdf".into(), press: [5; 16] }));
    files.emit(embed, ToParent::Done { files: 0 }.encode()).unwrap();
    assert_eq!(answer(&embeds, ask), Ok(()));

    // A refusal comes back as Files' words.
    let ask = pergola::files::show(&embeds, W_APP, SLOT, Folder::Downloads, "b.pdf", [6; 16]).unwrap();
    let embed = loop {
        if let ChildEvent::Attach { embed, .. } = next_c(&mut files) { break embed; }
    };
    files.emit(embed, ToParent::Failed { why: "Too many Files windows are open".into() }.encode()).unwrap();
    assert_eq!(answer(&embeds, ask), Err("Too many Files windows are open".into()));

    // A name with a folder in it is refused before anything is asked.
    assert!(pergola::files::show(&embeds, W_APP, SLOT, Folder::Downloads, "../secret", [7; 16]).is_err());
}
