//! Activation tokens (docs/spec/tabula.md §2): frescod's proof that a
//! user really pressed something in a window.
//!
//! Every key or button press a real device delivers to a window gets a
//! random 16-byte token, recorded here with the window, its app's uid and
//! what the press was, and sent to the app with the event. A broker the
//! app hands it to (Tabula, the share broker, Praeco) spends it here, once,
//! within a second, while that window is focused, and only on something
//! that press may authorise (typing never authorises a paste). Forum may
//! spend a press in its own window to grant a token to another app (a
//! click on a notification's action).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use fresco_protocol::activation::{kind, purpose, refusal, ConsumeResult, GrantResult, Token};

/// A token lives this long, or until spent.
pub const LIFETIME_NS: u64 = 1_000_000_000;

/// HID usages and modifier bits the purposes look at.
const HID_C: u16 = 0x06;
const HID_V: u16 = 0x19;
const HID_X: u16 = 0x1b;
const HID_ENTER: u16 = 0x28;
const HID_INSERT: u16 = 0x49;
const HID_KP_ENTER: u16 = 0x58;
const MOD_SHIFT: u8 = 1 << 0;
const MOD_CTRL: u8 = 1 << 1;
const MOD_ALT: u8 = 1 << 2;

/// At most this many live tokens: input rate bounds it far lower; past
/// it the oldest go (a flood of synthetic presses cannot grow it).
const MAX_LIVE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub window_id: u32,
    pub uid: u32,
    pub kind: u8,
    /// For `KEY`: the HID usage and modifiers.
    pub hid: u16,
    pub mods: u8,
    pub minted_ns: u64,
    pub counter: u64,
    /// For `DROP`: the drag it ends.
    pub drag_id: u64,
    /// For `GRANTED`: the one purpose it may be spent on.
    pub purpose: u8,
}

#[derive(Default)]
pub struct Table {
    live: HashMap<[u8; 16], Entry>,
    counter: u64,
}

fn refused(reason: u8) -> ConsumeResult {
    ConsumeResult::Refused { reason }
}

/// Whether a press of `e`'s kind may be spent on `purpose` (§2.2).
fn fits(e: &Entry, p: u8) -> bool {
    let ctrl = e.mods & MOD_CTRL != 0;
    match (e.kind, p) {
        (kind::GRANTED, p) => p == e.purpose,
        (kind::BUTTON, purpose::PASTE | purpose::COPY | purpose::DRAG | purpose::SHARE | purpose::ACTION) => true,
        (kind::KEY, purpose::PASTE) => (ctrl && e.hid == HID_V) || (e.mods & MOD_SHIFT != 0 && e.hid == HID_INSERT),
        (kind::KEY, purpose::COPY) => {
            ctrl || e.mods & MOD_ALT != 0 || e.hid == HID_ENTER || e.hid == HID_KP_ENTER || (ctrl && (e.hid == HID_C || e.hid == HID_X))
        }
        (kind::DROP, purpose::DROP) => true,
        // Arca's chip: a click, or Enter on the picked account.
        (kind::BUTTON, purpose::FILL) => true,
        (kind::KEY, purpose::FILL) => e.hid == HID_ENTER || e.hid == HID_KP_ENTER,
        _ => false,
    }
}

impl Table {
    /// A fresh token for `e` (its `counter` is set here).
    pub fn mint(&mut self, mut e: Entry) -> [u8; 16] {
        let now = e.minted_ns;
        self.live.retain(|_, v| now.saturating_sub(v.minted_ns) < LIFETIME_NS);
        if self.live.len() >= MAX_LIVE {
            if let Some(k) = self.live.iter().min_by_key(|(_, v)| v.counter).map(|(k, _)| *k) {
                self.live.remove(&k);
            }
        }
        self.counter += 1;
        e.counter = self.counter;
        let mut t = random();
        while t == [0; 16] || t == fresco_scene_server::window::MINT_HERE || self.live.contains_key(&t) {
            t = random();
        }
        self.live.insert(t, e);
        t
    }

    /// Spend `token` for `presenter_uid` on `purpose`; `focused(window)`
    /// says whether that window still has focus.
    /// For `FILL`, `visible(window, minted)` says whether that credential
    /// slot was fully visible and still before the press (arca-autofill.md §4).
    pub fn consume(&mut self, now: u64, token: &Token, presenter_uid: u32, purpose: u8, focused: impl Fn(u32) -> bool, visible: impl Fn(u32, u64) -> bool) -> ConsumeResult {
        let Some(e) = self.live.get(token).copied() else { return refused(refusal::UNKNOWN) };
        if now.saturating_sub(e.minted_ns) >= LIFETIME_NS {
            self.live.remove(token);
            return refused(refusal::EXPIRED);
        }
        // A failed check does not spend it: a wrong presenter cannot burn
        // the real app's token.
        if e.uid != presenter_uid {
            return refused(refusal::WRONG_UID);
        }
        if !fits(&e, purpose) {
            return refused(refusal::WRONG_KIND);
        }
        // Visibility first: a slot under a cover is OBSCURED whichever
        // window took focus.
        if purpose == purpose::FILL && !visible(e.window_id, e.minted_ns) {
            return refused(refusal::OBSCURED);
        }
        // A drop lands on whatever window is under the pointer, focused
        // or not; a granted token has no window.
        if !matches!(e.kind, kind::GRANTED | kind::DROP) && !focused(e.window_id) {
            return refused(refusal::NOT_FOCUSED);
        }
        self.live.remove(token);
        ConsumeResult::Ok { window_id: e.window_id, kind: e.kind, minted_ns: e.minted_ns, drag_id: e.drag_id }
    }

    /// Spend `token`, a button press in a window of `granter_uid`'s own,
    /// to mint a `GRANTED` token for `for_uid` to spend on `purpose`.
    pub fn grant(&mut self, now: u64, token: &Token, granter_uid: u32, for_uid: u32, purpose: u8) -> GrantResult {
        let Some(e) = self.live.get(token).copied() else { return GrantResult::Refused { reason: refusal::UNKNOWN } };
        if now.saturating_sub(e.minted_ns) >= LIFETIME_NS {
            self.live.remove(token);
            return GrantResult::Refused { reason: refusal::EXPIRED };
        }
        if e.uid != granter_uid {
            return GrantResult::Refused { reason: refusal::WRONG_UID };
        }
        if e.kind != kind::BUTTON || !matches!(purpose, purpose::PASTE | purpose::SHARE | purpose::ACTION) {
            return GrantResult::Refused { reason: refusal::WRONG_KIND };
        }
        self.live.remove(token);
        let g = self.mint(Entry { window_id: 0, uid: for_uid, kind: kind::GRANTED, hid: 0, mods: 0, minted_ns: now, counter: 0, drag_id: 0, purpose });
        GrantResult::Ok { token: g }
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }
}

fn random() -> [u8; 16] {
    let mut t = [0u8; 16];
    // SAFETY: arc4random_buf fills exactly `len` bytes of the buffer.
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    unsafe {
        libc::arc4random_buf(t.as_mut_ptr().cast(), t.len())
    };
    #[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut t)).expect("/dev/urandom");
    }
    t
}

/// Monotonic nanoseconds since frescod started.
pub fn now_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// frescod's one table.
pub fn table() -> &'static Mutex<Table> {
    static T: OnceLock<Mutex<Table>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Replace a real device's [`MINT_HERE`] marker on a press with a token
/// for the window's app (`uid`), or with zero when the app is not known.
///
/// [`MINT_HERE`]: fresco_scene_server::window::MINT_HERE
pub fn mint_for(ev: &mut fresco_scene_server::window::DisplayEvent, uid: Option<u32>) {
    use fresco_scene_server::window::{DisplayEvent as E, MINT_HERE};
    let (window_id, slot, kind, hid, mods) = match ev {
        E::InputKey { window_id, hid_usage, modifiers, activation, .. } => (*window_id, activation, kind::KEY, *hid_usage, *modifiers),
        E::InputPointerButton { window_id, modifiers, activation, .. } => (*window_id, activation, kind::BUTTON, 0, *modifiers),
        _ => return,
    };
    if *slot != MINT_HERE {
        return;
    }
    *slot = match uid {
        Some(uid) if window_id != 0 => table().lock().unwrap().mint(Entry {
            window_id, uid, kind, hid, mods, minted_ns: now_ns(), counter: 0, drag_id: 0, purpose: 0,
        }),
        _ => [0; 16],
    };
}

/// The same event with its token cleared: what the WM sees of a press in
/// another app's window.
/// A `DROP` token for `uid`'s window `window_id`, bound to `drag_id`.
pub fn mint_drop(window_id: u32, uid: u32, drag_id: u64) -> Token {
    table().lock().unwrap().mint(Entry { window_id, uid, kind: kind::DROP, hid: 0, mods: 0, minted_ns: now_ns(), counter: 0, drag_id, purpose: 0 })
}

pub fn without_token(ev: &fresco_scene_server::window::DisplayEvent) -> fresco_scene_server::window::DisplayEvent {
    use fresco_scene_server::window::DisplayEvent as E;
    let mut e = ev.clone();
    if let E::InputKey { activation, .. } | E::InputPointerButton { activation, .. } = &mut e {
        *activation = [0; 16];
    }
    e
}

/// Whether `ev` carries a token.
pub fn has_token(ev: &fresco_scene_server::window::DisplayEvent) -> bool {
    use fresco_scene_server::window::DisplayEvent as E;
    matches!(ev, E::InputKey { activation, .. } | E::InputPointerButton { activation, .. } if *activation != [0; 16])
}

/// The uids that may consume tokens: `FRESCOD_ACTIVATION_UIDS` (comma
/// separated uids), else the users `_tabula`, `_share` and `_praeco`.
/// Who may call the activation socket.
#[derive(Debug)]
pub struct Callers {
    pub consumers: Vec<u32>,
    pub forum: Option<u32>,
    /// May bind slots (limen.md §5): FRESCOD_SLOT_UIDS, else `_limen`'s.
    pub binders: Vec<u32>,
}

/// The uids that may bind slots: `FRESCOD_SLOT_UIDS` (comma separated),
/// else the user `_limen`.
pub fn binders() -> Vec<u32> {
    if let Ok(s) = std::env::var("FRESCOD_SLOT_UIDS") {
        return s.split(',').filter_map(|u| u.trim().parse().ok()).collect();
    }
    uid_of("_limen").into_iter().collect()
}

/// Answer `EMBED_CONNECT` on `conn` and, on `Ok`, hand `raw` (the same
/// stream) over as an embed-only client owned by the named uid
/// (fresco-video.md §8, S1). The caller sends nothing after it, so
/// nothing is left buffered in `conn`.
fn embed_connect(mut conn: aqueduct::Connection, raw: std::os::unix::net::UnixStream, payload: &[u8], binders: &[u32]) -> std::io::Result<()> {
    use fresco_protocol::slot::{op, refusal, EmbedConnect, EmbedConnectResult};
    let owner = match fresco_protocol::decode::<EmbedConnect>(payload) {
        Ok(e) if !binders.contains(&e.owner_uid) => Ok(e.owner_uid),
        _ => Err(refusal::INVALID),
    };
    let reply = match owner {
        Ok(_) => EmbedConnectResult::Ok,
        Err(reason) => EmbedConnectResult::Refused { reason },
    };
    let bytes = fresco_protocol::encode(&reply).unwrap_or_default();
    conn.send_message(aqueduct::CLASS_DISPLAY, op::EMBED_CONNECT, aqueduct::envelope::flag::IS_RESPONSE, &bytes)?;
    let Ok(owner) = owner else { return Ok(()) };
    drop(conn);
    match crate::socket_server::EMBED_CLIENT.get() {
        Some(serve) => serve(raw, owner),
        None => eprintln!("frescod: embed connection for uid {owner}: no client server"),
    }
    Ok(())
}

/// `slot::op::BIND`: find both windows by token, bind, and name their
/// owners so limend can check them against its own records.
pub fn bind_slot(comp: &Mutex<fresco_scene_server::window::Compositor>, b: &fresco_protocol::slot::SlotBind) -> fresco_protocol::slot::SlotBindResult {
    use fresco_protocol::slot::{refusal, SlotBindResult};
    let r = {
        let mut c = comp.lock().unwrap();
        let (Some(parent), Some(child)) = (c.window_by_token(&b.parent), c.window_by_token(&b.child)) else {
            return SlotBindResult::Refused { reason: refusal::UNKNOWN_WINDOW };
        };
        c.bind_slot(child, parent, b.kind, b.rect, b.z).map(|_| (parent, child))
    };
    match r {
        Ok((parent, child)) => SlotBindResult::Ok {
            parent_uid: crate::socket_server::window_owner_uid(parent as u32, comp).unwrap_or(0),
            child_uid: crate::socket_server::window_owner_uid(child as u32, comp).unwrap_or(0),
        },
        Err(reason) => SlotBindResult::Refused { reason },
    }
}

/// Forum's uid: FRESCOD_FORUM_UID, else `_forum`'s.
pub fn forum_uid() -> Option<u32> {
    match std::env::var("FRESCOD_FORUM_UID") {
        Ok(s) => s.trim().parse().ok(),
        Err(_) => uid_of("_forum"),
    }
}

pub fn consumers() -> Vec<u32> {
    if let Ok(s) = std::env::var("FRESCOD_ACTIVATION_UIDS") {
        return s.split(',').filter_map(|u| u.trim().parse().ok()).collect();
    }
    ["_tabula", "_share", "_praeco", "_arca-ui"].iter().filter_map(|n| uid_of(n)).collect()
}

fn uid_of(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam with a NUL-terminated name; the result is read at
    // once, before any other passwd call on this thread.
    let p = unsafe { libc::getpwnam(c.as_ptr()) };
    (!p.is_null()).then(|| unsafe { (*p).pw_uid })
}

/// The window that has the keyboard: the focused one, else the topmost
/// that is not a notification (what the input readers deliver keys to).
pub fn focused_window(comp: &Mutex<fresco_scene_server::window::Compositor>) -> u32 {
    comp.lock().unwrap().keyboard_window()
}

/// Serve the activation socket on FRESCOD_ACTIVATION_SOCK (default
/// /var/run/atrium/fresco-activation.sock, where tabulad looks).
pub fn start(comp: Arc<Mutex<fresco_scene_server::window::Compositor>>) {
    let path = std::env::var("FRESCOD_ACTIVATION_SOCK")
        .unwrap_or_else(|_| "/var/run/atrium/fresco-activation.sock".into());
    let path = std::path::Path::new(&path);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let allowed = Callers { consumers: consumers(), forum: forum_uid(), binders: binders() };
    let slots = comp.clone();
    let focused: Arc<dyn Fn(u32) -> bool + Send + Sync> = Arc::new(move |w| w != 0 && focused_window(&comp) == w);
    if let Err(e) = serve(path, allowed, focused, slots) {
        eprintln!("frescod: activation socket {}: {e}", path.display());
    }
}

/// Serve the activation socket on `path`: `CONSUME` for the uids in
/// `allowed`, `GRANT` for Forum's uid only; one aqueduct connection per
/// caller, peer uid checked on connect. `focused(window)` answers from
/// the compositor's focus.
pub fn serve(path: &std::path::Path, allowed: Callers, focused: Arc<dyn Fn(u32) -> bool + Send + Sync>, comp: Arc<Mutex<fresco_scene_server::window::Compositor>>) -> std::io::Result<()> {
    use std::os::unix::net::UnixListener;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        // Reachability (Portcullis mounts the directory only into the
        // brokers' jails) and the peer uid below are the gate.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }
    eprintln!("frescod: activation tokens on {} for uids {allowed:?}", path.display());
    std::thread::Builder::new().name("frescod-activation".into()).spawn(move || {
        for s in listener.incoming().flatten() {
            let uid = crate::socket_server::peer_xucred(&s).map(|(_, u)| u);
            let Some(uid) = uid.filter(|u| allowed.consumers.contains(u) || allowed.forum == Some(*u) || allowed.binders.contains(u)) else {
                eprintln!("frescod: activation socket: refused uid {uid:?}");
                continue;
            };
            let (focused, may_consume, may_grant) = (focused.clone(), allowed.consumers.contains(&uid), allowed.forum == Some(uid));
            let binder = allowed.binders.contains(&uid).then(|| comp.clone());
            let binders = allowed.binders.clone();
            let _ = std::thread::Builder::new().name("frescod-activation-conn".into()).spawn(move || {
                if let Err(e) = conn_loop(s, uid, may_consume, may_grant, &*focused, binder.as_deref(), &binders) {
                    log::debug!("activation connection: {e}");
                }
            });
        }
    })?;
    Ok(())
}

/// One caller's requests: `CONSUME` if it is a consumer, `GRANT` (with
/// its own uid as the granter) if it is Forum; anything else is refused.
fn conn_loop(s: std::os::unix::net::UnixStream, uid: u32, may_consume: bool, may_grant: bool, focused: &(dyn Fn(u32) -> bool + Send + Sync), binder: Option<&Mutex<fresco_scene_server::window::Compositor>>, binders: &[u32]) -> std::io::Result<()> {
    use aqueduct::envelope::flag;
    use fresco_protocol::activation::{op, Consume, Grant};
    let raw = s.try_clone()?;
    let mut conn = aqueduct::Connection::wrap(s)?;
    let mut first = true;
    loop {
        let msg = conn.recv_message()?;
        if msg.opcode_class != aqueduct::CLASS_DISPLAY {
            continue;
        }
        // A binder's new stream may become an embed-only client with its
        // first message (S1); later in a stream it is refused by silence.
        if std::mem::take(&mut first) && msg.op == fresco_protocol::slot::op::EMBED_CONNECT && binder.is_some() {
            return embed_connect(conn, raw, &msg.payload, binders);
        }
        let payload = match msg.op {
            op::CONSUME => fresco_protocol::encode(&match fresco_protocol::decode::<Consume>(&msg.payload) {
                Ok(c) if may_consume => {
                    let now = now_ns();
                    table().lock().unwrap().consume(now, &c.token, c.presenter_uid, c.purpose, focused, |w, minted| crate::credential::visible_for(w, minted, now))
                }
                Ok(_) => refused(refusal::NOT_ALLOWED),
                Err(_) => refused(refusal::UNKNOWN),
            }),
            op::GRANT => fresco_protocol::encode(&match fresco_protocol::decode::<Grant>(&msg.payload) {
                Ok(g) if may_grant => table().lock().unwrap().grant(now_ns(), &g.token, uid, g.for_uid, g.purpose),
                Ok(_) => GrantResult::Refused { reason: refusal::NOT_ALLOWED },
                Err(_) => GrantResult::Refused { reason: refusal::UNKNOWN },
            }),
            fresco_protocol::slot::op::BIND => {
                use fresco_protocol::slot::{refusal as no, SlotBind, SlotBindResult};
                fresco_protocol::encode(&match (fresco_protocol::decode::<SlotBind>(&msg.payload), binder) {
                    (Ok(b), Some(comp)) => bind_slot(comp, &b),
                    (Ok(_), None) => SlotBindResult::Refused { reason: no::NOT_ALLOWED },
                    (Err(_), _) => SlotBindResult::Refused { reason: no::INVALID },
                })
            }
            _ => continue,
        };
        conn.send_message(aqueduct::CLASS_DISPLAY, msg.op, flag::IS_RESPONSE, &payload.unwrap_or_default())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(hid: u16, mods: u8, at: u64) -> Entry {
        Entry { window_id: 5, uid: 1001, kind: kind::KEY, hid, mods, minted_ns: at, counter: 0, drag_id: 0, purpose: 0 }
    }
    fn button(at: u64) -> Entry {
        Entry { kind: kind::BUTTON, hid: 0, mods: 0, ..key(0, 0, at) }
    }
    const YES: fn(u32) -> bool = |_| true;
    const NOVIS: fn(u32, u64) -> bool = |_, _| false;

    /// 0 when spent, else the refusal.
    fn why(r: ConsumeResult) -> u8 {
        match r {
            ConsumeResult::Ok { .. } => 0,
            ConsumeResult::Refused { reason } => reason,
        }
    }
    fn granted(r: GrantResult) -> Result<Token, u8> {
        match r {
            GrantResult::Ok { token } => Ok(token),
            GrantResult::Refused { reason } => Err(reason),
        }
    }

    #[test]
    fn a_token_is_spent_once_by_its_own_app_within_a_second() {
        let mut t = Table::default();
        let a = t.mint(button(0));
        assert_ne!(a, [0; 16]);
        assert_eq!(why(t.consume(10, &a, 1002, purpose::PASTE, YES, NOVIS)), refusal::WRONG_UID, "another app");
        let r = t.consume(10, &a, 1001, purpose::PASTE, YES, NOVIS);
        assert!(matches!(r, ConsumeResult::Ok { window_id: 5, kind: kind::BUTTON, .. }));
        assert_eq!(why(t.consume(11, &a, 1001, purpose::PASTE, YES, NOVIS)), refusal::UNKNOWN, "replayed");
        let b = t.mint(button(0));
        assert_eq!(why(t.consume(LIFETIME_NS, &b, 1001, purpose::PASTE, YES, NOVIS)), refusal::EXPIRED, "expired");
        let c = t.mint(button(0));
        assert_eq!(why(t.consume(5, &c, 1001, purpose::PASTE, |_| false, NOVIS)), refusal::NOT_FOCUSED, "window no longer focused");
        assert_eq!(why(t.consume(6, &c, 1001, purpose::PASTE, YES, NOVIS)), 0, "a refusal did not spend it");
    }

    #[test]
    fn typing_authorises_nothing_but_the_chords() {
        let mut t = Table::default();
        let typed = t.mint(key(0x04, 0, 0)); // 'a'
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::PASTE, YES, NOVIS)), refusal::WRONG_KIND);
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::COPY, YES, NOVIS)), refusal::WRONG_KIND);
        let v = t.mint(key(HID_V, 0, 0)); // a bare 'v'
        assert_eq!(why(t.consume(1, &v, 1001, purpose::PASTE, YES, NOVIS)), refusal::WRONG_KIND);
        let ctrl_v = t.mint(key(HID_V, MOD_CTRL, 0));
        assert_eq!(why(t.consume(1, &ctrl_v, 1001, purpose::PASTE, YES, NOVIS)), 0);
        let shift_ins = t.mint(key(HID_INSERT, MOD_SHIFT, 0));
        assert_eq!(why(t.consume(1, &shift_ins, 1001, purpose::PASTE, YES, NOVIS)), 0);
        let ctrl_c = t.mint(key(HID_C, MOD_CTRL, 0));
        assert_eq!(why(t.consume(1, &ctrl_c, 1001, purpose::PASTE, YES, NOVIS)), refusal::WRONG_KIND, "a copy chord is not a paste");
        assert_eq!(why(t.consume(1, &ctrl_c, 1001, purpose::COPY, YES, NOVIS)), 0);
        let ctrl_v2 = t.mint(key(HID_V, MOD_CTRL, 0));
        assert_eq!(why(t.consume(1, &ctrl_v2, 1001, purpose::DROP, YES, NOVIS)), refusal::WRONG_KIND);
    }

    #[test]
    fn forum_grants_from_its_own_press_for_one_purpose() {
        let mut t = Table::default();
        let forum = 900;
        let press = t.mint(Entry { uid: forum, ..button(0) });
        assert_eq!(granted(t.grant(1, &press, 1001, 1001, purpose::ACTION)), Err(refusal::WRONG_UID), "not the granter's press");
        let app_press = t.mint(button(0));
        assert_eq!(granted(t.grant(1, &app_press, forum, 1001, purpose::ACTION)), Err(refusal::WRONG_UID), "an app's press");
        assert_eq!(granted(t.grant(1, &press, forum, 1001, purpose::COPY)), Err(refusal::WRONG_KIND));
        let g = granted(t.grant(1, &press, forum, 1001, purpose::ACTION)).unwrap();
        assert_ne!(g, [0; 16]);
        assert_eq!(granted(t.grant(2, &press, forum, 1001, purpose::ACTION)), Err(refusal::UNKNOWN), "the press is spent");
        assert_eq!(why(t.consume(2, &g, 1001, purpose::PASTE, YES, NOVIS)), refusal::WRONG_KIND, "only its purpose");
        let r = t.consume(2, &g, 1001, purpose::ACTION, |_| false, NOVIS);
        assert!(matches!(r, ConsumeResult::Ok { kind: kind::GRANTED, .. }), "granted tokens need no focus");
    }

    #[test]
    fn a_fill_needs_a_click_or_enter_in_a_slot_seen_whole() {
        let mut t = Table::default();
        let click = t.mint(button(0));
        assert_eq!(why(t.consume(1, &click, 1001, purpose::FILL, YES, NOVIS)), refusal::OBSCURED);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::FILL, YES, |w, m| w == 5 && m == 0)), 0, "the refusal did not spend it");
        let enter = t.mint(key(HID_ENTER, 0, 0));
        assert_eq!(why(t.consume(1, &enter, 1001, purpose::FILL, YES, |_, _| true)), 0);
        let typed = t.mint(key(0x04, 0, 0));
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::FILL, YES, |_, _| true)), refusal::WRONG_KIND);
    }

    #[test]
    fn the_table_stays_bounded() {
        let mut t = Table::default();
        for i in 0..(MAX_LIVE as u64 + 100) {
            t.mint(button(i)); // all within the second
        }
        assert!(t.len() <= MAX_LIVE);
        t.mint(button(LIFETIME_NS * 3));
        assert_eq!(t.len(), 1, "expired ones go on the next mint");
    }
}
