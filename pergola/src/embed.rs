//! A window's embeds, through Limen (docs/spec/limen.md §11).
//!
//! [`Embeds`] holds the window's connection to limend and which slot is
//! which embed. Give the `Window` an [`Embeds::binder`] so slot rects reach
//! limend as layout moves them, and poll it in the app's loop for the
//! children's messages.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

pub use limen::client::Error;
pub use limen::proto::{EmbedId, ParentEvent, WindowToken};

use crate::slot::{SlotBinder, SlotKey};

pub struct Embeds {
    parent: limen::client::Parent,
    by_slot: HashMap<SlotKey, EmbedId>,
    /// Each slot's last rect from layout, so a request made after the slot
    /// was laid out places it at once.
    rects: HashMap<SlotKey, [f32; 4]>,
}

impl Embeds {
    /// limend at `LIMEN_SOCKET`, else its default socket.
    pub fn connect() -> Result<Rc<RefCell<Self>>, Error> {
        Ok(Self::wrap(limen::client::Parent::connect()?))
    }

    pub fn wrap(parent: limen::client::Parent) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self { parent, by_slot: HashMap::new(), rects: HashMap::new() }))
    }

    /// Ask for a `role` child in the slot `slot` of the window named by
    /// `window` (frescod's embed token for it).
    pub fn request(&mut self, role: &str, window: WindowToken, slot: SlotKey) -> Result<EmbedId, Error> {
        let e = self.parent.request(role, window)?;
        self.by_slot.insert(slot, e);
        if let Some(r) = self.rects.get(&slot).copied() {
            self.parent.place(e, Some(r))?;
        }
        Ok(e)
    }

    pub fn embed_of(&self, slot: SlotKey) -> Option<EmbedId> { self.by_slot.get(&slot).copied() }

    /// A role message to the child (encode it with the role's types).
    pub fn send(&mut self, embed: EmbedId, bytes: Vec<u8>) -> Result<(), Error> {
        self.parent.send(embed, bytes)
    }

    pub fn detach(&mut self, slot: SlotKey) -> Result<(), Error> {
        match self.by_slot.remove(&slot) {
            Some(e) => self.parent.detach(e),
            None => Ok(()),
        }
    }

    /// The next event, if one comes within `wait`. A lost or detached
    /// child's slot is forgotten.
    pub fn poll(&mut self, wait: Duration) -> Result<Option<ParentEvent>, Error> {
        let ev = self.parent.poll(wait)?;
        if let Some(ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed }) = &ev {
            self.by_slot.retain(|_, e| e != embed);
        }
        Ok(ev)
    }

    /// The `Window`'s slot binder for these embeds.
    pub fn binder(this: &Rc<RefCell<Self>>) -> Box<dyn SlotBinder> {
        Box::new(Binder(this.clone()))
    }
}

struct Binder(Rc<RefCell<Embeds>>);

impl SlotBinder for Binder {
    fn place(&mut self, _window_id: u32, key: SlotKey, rect: Option<[f32; 4]>) {
        let mut e = self.0.borrow_mut();
        match rect {
            Some(r) => { e.rects.insert(key, r); }
            None => { e.rects.remove(&key); }
        }
        if let Some(id) = e.embed_of(key) {
            if let Err(err) = e.parent.place(id, rect) {
                eprintln!("pergola: placing embed {id}: {err:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Against the real limend code, with fake frescod and Arca.
    use super::*;
    use crate::app::App;
    use crate::reactive::Mutable;
    use crate::view::{Ctx, View};
    use crate::widgets::{CredentialField, CredentialState};
    use limen::catalogue::{self, SlotKind};
    use limen::credential::ToParent;
    use limen::proto::ChildEvent;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    const APP: u32 = 1001;
    const ARCA: u32 = 900;
    const W: WindowToken = [1; 16];
    const W_ARCA: WindowToken = [3; 16];

    #[derive(Clone, Default)]
    struct Fresco(Arc<Mutex<Vec<Option<[f32; 4]>>>>);
    impl limen::daemon::Compositor for Fresco {
        fn bind(&mut self, p: WindowToken, c: WindowToken, k: SlotKind, r: Option<[f32; 4]>) -> Result<(u32, u32), u8> {
            assert_eq!((p, c, k), (W, W_ARCA, SlotKind::Credential));
            self.0.lock().unwrap().push(r);
            Ok((APP, ARCA))
        }
    }
    struct NoLaunch;
    impl limen::daemon::Launcher for NoLaunch {
        fn launch(&mut self, _: &str, _: EmbedId, _: UnixStream) -> Result<u32, String> { Err("none".into()) }
        fn end(&mut self, _: EmbedId) {}
    }

    struct Form { f: CredentialField }
    impl View for Form {
        fn render(&self, ctx: &mut Ctx) { self.f.render(ctx) }
    }

    fn conn(shared: &Arc<limen::daemon::Shared>, uid: u32) -> aqueduct::Connection {
        let (a, b) = UnixStream::pair().unwrap();
        let sh = shared.clone();
        std::thread::spawn(move || { let _ = limen::daemon::serve(aqueduct::Connection::wrap(b).unwrap(), uid, sh); });
        aqueduct::Connection::wrap(a).unwrap()
    }

    #[test]
    fn a_credential_field_gets_arcas_slot_and_its_messages() {
        let fresco = Fresco::default();
        let resolve = Box::new(|uid: u32| match uid {
            APP => Some(("ana".into(), "org.example.mail".into())),
            ARCA => Some(("ana".into(), catalogue::ARCA.into())),
            _ => None,
        });
        let shared = limen::daemon::Shared::new(Box::new(fresco.clone()), Box::new(NoLaunch), resolve, Box::new(|_: &str, _: &str| None));
        let mut arca = limen::client::Child::wrap(conn(&shared, ARCA));
        arca.provide_role("credential").unwrap();

        let state = Mutable::new(CredentialState::default());
        let field = CredentialField::password(state.clone());
        let key = field.slot_key();
        let mut app = App::new(Form { f: field.clone() });
        let embeds = Embeds::wrap(limen::client::Parent::wrap(conn(&shared, APP)));
        let mut binder = Embeds::binder(&embeds);
        // Laid out before the request: the request places it.
        app.tick();
        for (k, r) in app.take_slot_changes() {
            binder.place(7, k, r.map(|r| [r.x(), r.y(), r.w(), r.h()]));
        }
        let e = embeds.borrow_mut().request("credential", W, key).unwrap();
        let Some(ChildEvent::Attach { embed, .. }) = arca.poll(Duration::from_secs(2)).unwrap() else { panic!() };
        assert_eq!(embed, e);
        arca.ready(e, W_ARCA).unwrap();
        assert_eq!(embeds.borrow_mut().poll(Duration::from_secs(2)).unwrap(), Some(ParentEvent::Attached { embed: e }));
        assert_eq!(fresco.0.lock().unwrap().last().copied().flatten(), Some([0.0, 0.0, 240.0, crate::theme::size::INPUT_HEIGHT_DEFAULT]));

        arca.emit(e, ToParent::Handle { handle: [5; 16] }.encode()).unwrap();
        match embeds.borrow_mut().poll(Duration::from_secs(2)).unwrap() {
            Some(ParentEvent::Message { embed, bytes }) if embed == e => field.apply(ToParent::decode(&bytes).unwrap()),
            other => panic!("{other:?}"),
        }
        assert_eq!(state.get_cloned().handle, Some([5; 16]));
    }
}
