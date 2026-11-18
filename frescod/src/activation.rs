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

/// The lifetime in force: [`LIFETIME_NS`], except that a VM under TCG
/// (50-100x slow) that injects its presses (`FRESCOD_INJECT_ACTIVATION=1`,
/// testing only) may stretch it with `FRESCOD_TOKEN_LIFE_MS` (at most 30 s).
fn lifetime() -> u64 {
    static L: OnceLock<u64> = OnceLock::new();
    *L.get_or_init(|| {
        let testing = std::env::var("FRESCOD_INJECT_ACTIVATION").as_deref() == Ok("1");
        match std::env::var("FRESCOD_TOKEN_LIFE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            Some(ms) if testing => {
                eprintln!("frescod: WARNING: activation tokens live {ms} ms (FRESCOD_TOKEN_LIFE_MS, testing only)");
                ms.clamp(1, 30_000) * 1_000_000
            }
            _ => LIFETIME_NS,
        }
    })
}

/// HID usages and modifier bits the purposes look at.
const HID_C: u16 = 0x06;
const HID_V: u16 = 0x19;
const HID_X: u16 = 0x1b;
const HID_ENTER: u16 = 0x28;
const HID_INSERT: u16 = 0x49;
const HID_N: u16 = 0x11;
const HID_T: u16 = 0x17;
const HID_KP_ENTER: u16 = 0x58;
/// The HID modifier byte (either hand), as the wire carries it.
const MOD_CTRL: u8 = 0x01 | 0x10;
const MOD_SHIFT: u8 = 0x02 | 0x20;
const MOD_ALT: u8 = 0x04 | 0x40;

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
    /// When a real key or button press last reached each window (ns), for
    /// `slot::op::PRESSED`; dropped with the window's tokens' bound.
    pressed: HashMap<u32, u64>,
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
        // Sigillum's sheet: the same, in the sheet's own window.
        (kind::BUTTON, purpose::IDENTITY) => true,
        (kind::KEY, purpose::IDENTITY) => e.hid == HID_ENTER || e.hid == HID_KP_ENTER,
        // Opening a file: a double-click or Open, or Enter on the row.
        (kind::BUTTON, purpose::OPEN) => true,
        (kind::KEY, purpose::OPEN) => e.hid == HID_ENTER || e.hid == HID_KP_ENTER,
        // An app's console: Terminal pressed, or Enter on it, in the
        // launcher; in the terminal, New Tab or New Window, or their keys.
        (kind::BUTTON, purpose::CONSOLE) => true,
        (kind::KEY, purpose::CONSOLE) => {
            e.hid == HID_ENTER || e.hid == HID_KP_ENTER
                || (ctrl && e.mods & MOD_SHIFT != 0 && (e.hid == HID_T || e.hid == HID_N))
        }
        // Allowing an app from Settings: Allow pressed, or Enter on it.
        (kind::BUTTON, purpose::APPROVE) => true,
        (kind::KEY, purpose::APPROVE) => e.hid == HID_ENTER || e.hid == HID_KP_ENTER,
        // A prompt's answer (navigator-ui.md §6.4): a click, or Enter.
        (kind::BUTTON, purpose::PROMPT) => true,
        (kind::KEY, purpose::PROMPT) => e.hid == HID_ENTER || e.hid == HID_KP_ENTER,
        // Opening a link: a click, or Enter.
        (kind::BUTTON, purpose::LINK) => true,
        (kind::KEY, purpose::LINK) => e.hid == HID_ENTER || e.hid == HID_KP_ENTER,
        _ => false,
    }
}

impl Table {
    /// A fresh token for `e` (its `counter` is set here).
    pub fn mint(&mut self, mut e: Entry) -> [u8; 16] {
        let now = e.minted_ns;
        let life = if cfg!(test) { LIFETIME_NS } else { lifetime() };
        self.live.retain(|_, v| now.saturating_sub(v.minted_ns) < life);
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
    /// For `FILL`, `IDENTITY` and `APPROVE`, `visible(window, minted)` says
    /// whether that credential slot, sheet or window was fully visible and
    /// still before the press (arca-autofill.md §4, sigillum.md §7.2,
    /// curia.md §13); for `PROMPT`, asked of `window | credential::GUARD`,
    /// whether the window's guard was.
    pub fn consume(&mut self, now: u64, token: &Token, presenter_uid: u32, purpose: u8, focused: impl Fn(u32) -> bool, visible: impl Fn(u32, u64) -> bool) -> ConsumeResult {
        let Some(e) = self.live.get(token).copied() else { return refused(refusal::UNKNOWN) };
        if now.saturating_sub(e.minted_ns) >= if cfg!(test) { LIFETIME_NS } else { lifetime() } {
            self.live.remove(token);
            eprintln!("frescod: activation: a token {} ms old refused", now.saturating_sub(e.minted_ns) / 1_000_000);
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
        // A prompt's answer: the window's guard, not the window.
        // A link: the window's guard too, if it has one (`visible` answers
        // for an unguarded window's `LINK` press).
        let watched = if matches!(purpose, purpose::PROMPT | purpose::LINK) { e.window_id | crate::credential::GUARD } else { e.window_id };
        if matches!(purpose, purpose::FILL | purpose::IDENTITY | purpose::APPROVE | purpose::PROMPT | purpose::LINK) && !visible(watched, e.minted_ns) {
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
        // The same lifetime as a consume (FRESCOD_TOKEN_LIFE_MS in tests).
        if now.saturating_sub(e.minted_ns) >= if cfg!(test) { LIFETIME_NS } else { lifetime() } {
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
        Some(uid) if window_id != 0 => {
            let mut t = table().lock().unwrap();
            let now = now_ns();
            t.pressed_at(window_id, now);
            t.mint(Entry { window_id, uid, kind, hid, mods, minted_ns: now, counter: 0, drag_id: 0, purpose: 0 })
        }
        _ => [0; 16],
    };
}

impl Table {
    /// A press reached `window_id` at `now`.
    pub fn pressed_at(&mut self, window_id: u32, now: u64) {
        if self.pressed.len() >= MAX_LIVE && !self.pressed.contains_key(&window_id) {
            // Bounded like the tokens: the stalest go first.
            let horizon = now.saturating_sub(u64::from(fresco_protocol::slot::PRESSED_MAX_MS) * 1_000_000);
            self.pressed.retain(|_, &mut t| t >= horizon);
            if self.pressed.len() >= MAX_LIVE {
                return;
            }
        }
        self.pressed.insert(window_id, now);
    }

    /// Whether a press reached `window_id` within `within_ms` of `now`
    /// (at most `PRESSED_MAX_MS`).
    pub fn pressed_within(&self, window_id: u32, now: u64, within_ms: u32) -> bool {
        let ms = within_ms.min(fresco_protocol::slot::PRESSED_MAX_MS);
        self.pressed.get(&window_id).is_some_and(|&t| now.saturating_sub(t) <= u64::from(ms) * 1_000_000)
    }
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

/// Who may call the activation socket.
#[derive(Debug)]
pub struct Callers {
    /// Consumers and the purposes each may spend (`None`: any, a test
    /// harness's bare uid in `FRESCOD_ACTIVATION_UIDS`).
    pub consumers: Consumers,
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
        c.bind_slot_keep(child, parent, b.kind, b.rect, b.z, b.keep_shown).map(|_| (parent, child))
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

/// Consumer uids and what each may spend (`None`: any purpose).
pub type Consumers = std::collections::BTreeMap<u32, Option<Vec<u8>>>;

/// Each broker and the purposes it spends: a broker can't spend a press
/// meant for another (a compromised navigatord can't paste or open a file).
pub const DEFAULT_CONSUMERS: &[(&str, &[u8])] = &[
    // portcullisd (opening-files.md §6.2, homes.md §4, curia.md §13).
    ("root", &[purpose::OPEN, purpose::CONSOLE, purpose::APPROVE, purpose::LINK]),
    ("_tabula", &[purpose::COPY, purpose::PASTE, purpose::DRAG, purpose::DROP]),
    ("_share", &[purpose::SHARE]),
    ("_praeco", &[purpose::ACTION]),
    ("_arca-ui", &[purpose::FILL]),
    ("_sigillum-ui", &[purpose::IDENTITY]),
];

/// A purpose by its name in `FRESCOD_ACTIVATION_UIDS`, or its number.
fn purpose_named(n: &str) -> Option<u8> {
    Some(match n.to_ascii_lowercase().as_str() {
        "copy" => purpose::COPY,
        "paste" => purpose::PASTE,
        "drop" => purpose::DROP,
        "drag" => purpose::DRAG,
        "share" => purpose::SHARE,
        "action" => purpose::ACTION,
        "fill" => purpose::FILL,
        "identity" => purpose::IDENTITY,
        "open" => purpose::OPEN,
        "console" => purpose::CONSOLE,
        "approve" => purpose::APPROVE,
        "prompt" => purpose::PROMPT,
        "link" => purpose::LINK,
        n => return n.parse().ok(),
    })
}

/// `FRESCOD_ACTIVATION_UIDS`: comma-separated `uid=purpose+purpose`
/// (names or numbers), or a bare `uid` for any purpose (test harnesses).
/// An entry that doesn't parse is dropped, whole.
pub fn parse_consumers(s: &str) -> Consumers {
    let mut out = Consumers::new();
    for e in s.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (u, ps) = match e.split_once('=') {
            Some((u, ps)) => (u, Some(ps)),
            None => (e, None),
        };
        let Ok(uid) = u.trim().parse::<u32>() else { continue };
        let ps = match ps {
            None => None,
            Some(ps) => match ps.split('+').map(|p| purpose_named(p.trim())).collect::<Option<Vec<u8>>>() {
                Some(v) => Some(v),
                None => continue,
            },
        };
        merge(&mut out, uid, ps);
    }
    out
}

fn merge(out: &mut Consumers, uid: u32, ps: Option<Vec<u8>>) {
    match (out.get_mut(&uid), ps) {
        (None, ps) => {
            out.insert(uid, ps);
        }
        (Some(have), None) => *have = None,
        (Some(None), Some(_)) => {}
        (Some(Some(have)), Some(more)) => have.extend(more.into_iter().filter(|p| !have.contains(p)).collect::<Vec<_>>()),
    }
}

/// Whether a consumer with `may` may spend on `p`.
/// Purposes a program may spend on its own windows' presses without being
/// a consumer: the press is still checked here (guard, stillness, focus),
/// and only the window's owner can spend it. The Navigator's prompt row is
/// answered by the person's own navigatord (navigator-ui.md §6.4).
pub const SELF_SPENT: &[u8] = &[purpose::PROMPT];

/// `uid` spends a press of its own window on a self-spent purpose.
pub fn self_spend(uid: u32, c: &fresco_protocol::activation::Consume) -> bool {
    c.presenter_uid == uid && SELF_SPENT.contains(&c.purpose)
}

pub fn may_spend(may: &Option<Vec<u8>>, p: u8) -> bool {
    may.as_ref().is_none_or(|v| v.contains(&p))
}

pub fn consumers() -> Consumers {
    if let Ok(s) = std::env::var("FRESCOD_ACTIVATION_UIDS") {
        let c = parse_consumers(&s);
        for (u, _) in c.iter().filter(|(_, p)| p.is_none()) {
            eprintln!("frescod: WARNING: uid {u} may spend activation tokens on any purpose (FRESCOD_ACTIVATION_UIDS, testing)");
        }
        return c;
    }
    let mut c = Consumers::new();
    for (name, ps) in DEFAULT_CONSUMERS {
        if let Some(u) = uid_of(name) {
            merge(&mut c, u, Some(ps.to_vec()));
        }
    }
    c
}

/// The uids whose top-level windows are trusted sheets, watched for
/// `IDENTITY` presses like credential slots: `FRESCOD_SHEET_UIDS` (comma
/// separated), else the user `_sigillum-ui`. Read once.
pub fn sheet_uids() -> &'static [u32] {
    static UIDS: OnceLock<Vec<u32>> = OnceLock::new();
    UIDS.get_or_init(|| match std::env::var("FRESCOD_SHEET_UIDS") {
        Ok(s) => s.split(',').filter_map(|u| u.trim().parse().ok()).collect(),
        Err(_) => uid_of("_sigillum-ui").into_iter().collect(),
    })
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

/// Where the activation socket is by default: a directory of its own under
/// `/atrium/sockets`, so a capability can mount it into a jail
/// (navigator-ui.md §2.3a).
pub const DEFAULT_SOCK: &str = "/atrium/sockets/fresco-activation/fresco-activation.sock";
/// Its old place, where its clients look by default: a link to it.
pub const OLD_SOCK: &str = "/var/run/atrium/fresco-activation.sock";

/// Serve the activation socket on FRESCOD_ACTIVATION_SOCK, else
/// [`DEFAULT_SOCK`] with a link at [`OLD_SOCK`].
pub fn start(comp: Arc<Mutex<fresco_scene_server::window::Compositor>>) {
    let set = std::env::var("FRESCOD_ACTIVATION_SOCK").ok().filter(|p| !p.is_empty());
    let default = set.is_none();
    let path = set.unwrap_or_else(|| DEFAULT_SOCK.into());
    let path = std::path::Path::new(&path);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if default {
        link_old(path, std::path::Path::new(OLD_SOCK));
    }
    let allowed = Callers { consumers: consumers(), forum: forum_uid(), binders: binders() };
    let slots = comp.clone();
    let focused: Arc<dyn Fn(u32) -> bool + Send + Sync> = Arc::new(move |w| w != 0 && focused_window(&comp) == w);
    if let Err(e) = serve(path, allowed, focused, slots) {
        eprintln!("frescod: activation socket {}: {e}", path.display());
    }
}

/// A link at `old` to `new`, replacing whatever was there.
pub fn link_old(new: &std::path::Path, old: &std::path::Path) {
    if let Some(d) = old.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::remove_file(old);
    if let Err(e) = std::os::unix::fs::symlink(new, old) {
        eprintln!("frescod: link {} -> {}: {e}", old.display(), new.display());
    }
}

/// Serve the activation socket on `path`: `CONSUME` for the uids in
/// `allowed`, `GRANT` for Forum's uid only; one aqueduct connection per
/// caller, peer uid checked on connect. `focused(window)` answers from
/// the compositor's focus.
pub fn serve(path: &std::path::Path, allowed: Callers, focused: Arc<dyn Fn(u32) -> bool + Send + Sync>, comp: Arc<Mutex<fresco_scene_server::window::Compositor>>) -> std::io::Result<()> {
    // The supervisor's socket when it bound one (fast-boot.md §5), else ours.
    let (listener, origin) = atrium_listen::listener(path)?;
    if origin == atrium_listen::Origin::Bound {
        use std::os::unix::fs::PermissionsExt;
        // Reachability (Portcullis mounts the directory only into the
        // brokers' jails) and the peer uid below are the gate.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }
    eprintln!("frescod: activation tokens on {} for uids {allowed:?}", path.display());
    std::thread::Builder::new().name("frescod-activation".into()).spawn(move || {
        for s in listener.incoming().flatten() {
            let uid = crate::socket_server::peer_xucred(&s).map(|(_, u)| u);
            // Any uid may connect: one that is no consumer may still spend
            // presses on its own windows for a self-spent purpose (`PROMPT`).
            let Some(uid) = uid else {
                eprintln!("frescod: activation socket: no peer uid");
                continue;
            };
            let (focused, may_consume, may_grant) = (focused.clone(), allowed.consumers.get(&uid).cloned(), allowed.forum == Some(uid));
            let binder = allowed.binders.contains(&uid).then(|| comp.clone());
            let binders = allowed.binders.clone();
            let all = comp.clone();
            let _ = std::thread::Builder::new().name("frescod-activation-conn".into()).spawn(move || {
                if let Err(e) = conn_loop(s, uid, may_consume, may_grant, &*focused, binder.as_deref(), &binders, &all) {
                    log::debug!("activation connection: {e}");
                }
            });
        }
    })?;
    Ok(())
}

/// Strip notes owed for `PASTE` and `DROP` presses spent (spender uid,
/// window, when), one each for [`NOTE_FOR_NS`], and the note showing.
#[derive(Default)]
pub struct Notes {
    owed: Vec<(u32, u32, u64)>,
    shown: Option<(u32, String, u64)>,
}

/// How long after the press a broker may ask for its note.
pub const NOTE_FOR_NS: u64 = 5_000_000_000;
/// How long a note shows.
pub const NOTE_SHOWN_NS: u64 = 4_000_000_000;

impl Notes {
    pub fn owe(&mut self, uid: u32, window: u32, now: u64) {
        self.owed.retain(|&(_, _, at)| now.saturating_sub(at) < NOTE_FOR_NS);
        if self.owed.len() < 64 {
            self.owed.push((uid, window, now));
        }
    }

    /// Show `n` if `uid` spent a press for its window lately (once).
    pub fn show(&mut self, uid: u32, n: &fresco_protocol::activation::Note, now: u64) -> bool {
        self.owed.retain(|&(_, _, at)| now.saturating_sub(at) < NOTE_FOR_NS);
        let Some(i) = self.owed.iter().position(|&(u, w, _)| u == uid && w == n.window_id) else { return false };
        let Some(text) = note_text(n) else { return false };
        self.owed.remove(i);
        self.shown = Some((n.window_id, text, now + NOTE_SHOWN_NS));
        true
    }

    /// The note over `window`, while it shows.
    pub fn over(&self, window: u32, now: u64) -> Option<String> {
        self.shown.as_ref().filter(|&&(w, _, until)| w == window && now < until).map(|(_, t, _)| t.clone())
    }
}

/// A note's words, frescod's own: only the persona's name comes from the
/// broker, cut to the strip's characters.
pub fn note_text(n: &fresco_protocol::activation::Note) -> Option<String> {
    let persona: String = n.persona.to_ascii_lowercase().bytes()
        .filter(|&c| fresco_split::chrome::origin_char(c)).take(32).map(char::from).collect();
    match n.kind {
        fresco_protocol::activation::note::COPIED_IN if !persona.is_empty() => Some(format!("paste refused: copied in {persona}")),
        _ => None,
    }
}

fn notes() -> &'static Mutex<Notes> {
    static N: std::sync::OnceLock<Mutex<Notes>> = std::sync::OnceLock::new();
    N.get_or_init(Default::default)
}

/// The strip note over `window`, if one shows now.
pub fn strip_note(window: u32) -> Option<String> {
    notes().lock().unwrap().over(window, now_ns())
}

/// The display loop's redraw signal, so a note shows and goes on time.
static REDRAW: std::sync::OnceLock<crate::redraw::Redraw> = std::sync::OnceLock::new();

pub fn set_redraw(r: crate::redraw::Redraw) {
    let _ = REDRAW.set(r);
}

fn note_shown() {
    if let Some(r) = REDRAW.get() {
        r.wake();
        let r = r.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_nanos(NOTE_SHOWN_NS + 50_000_000));
            r.wake();
        });
    }
}

/// Raises owed for `LINK` presses spent: (spender uid, when), one each,
/// for two minutes (a link grant's life, opening-links.md §5).
#[derive(Default)]
pub struct Raises(Vec<(u32, u64)>);

pub const RAISE_FOR_NS: u64 = 120_000_000_000;

impl Raises {
    pub fn add(&mut self, uid: u32, now: u64) {
        self.0.retain(|&(_, at)| now.saturating_sub(at) < RAISE_FOR_NS);
        if self.0.len() < 64 {
            self.0.push((uid, now));
        }
    }

    /// Take one of `uid`'s, if it has one still fresh.
    pub fn take(&mut self, uid: u32, now: u64) -> bool {
        self.0.retain(|&(_, at)| now.saturating_sub(at) < RAISE_FOR_NS);
        match self.0.iter().position(|&(u, _)| u == uid) {
            Some(i) => {
                self.0.remove(i);
                true
            }
            None => false,
        }
    }
}

fn raises() -> &'static Mutex<Raises> {
    static R: std::sync::OnceLock<Mutex<Raises>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// The window `uid` would want forward: its top-level window highest in
/// the stack (not a slot, a pop-up or hidden), and the WM is told to
/// bring it forward.
fn raise(comp: &Mutex<fresco_scene_server::window::Compositor>, uid: u32) -> fresco_protocol::activation::RaiseResult {
    use fresco_protocol::activation::RaiseResult;
    let c = comp.lock().unwrap();
    let owner = |w: &fresco_scene_server::window::Window| crate::socket_server::client_uid(w.owner).unwrap_or(w.owner_uid);
    let top = c.windows.values()
        .filter(|w| w.slot.is_none() && w.popup.is_none() && !w.hidden && w.kept_by.is_none() && owner(w) == uid)
        .max_by_key(|w| w.z)
        .map(|w| u32::from(w.id));
    match top {
        Some(window_id) => {
            c.emit(fresco_scene_server::window::DisplayEvent::WindowActivate { window_id });
            RaiseResult::Ok { window_id }
        }
        None => RaiseResult::Refused { reason: refusal::UNKNOWN },
    }
}

/// One caller's requests: `CONSUME` on its purposes if it is a consumer, `GRANT` (with
/// its own uid as the granter) if it is Forum; anything else is refused.
fn conn_loop(s: std::os::unix::net::UnixStream, uid: u32, may_consume: Option<Option<Vec<u8>>>, may_grant: bool, focused: &(dyn Fn(u32) -> bool + Send + Sync), binder: Option<&Mutex<fresco_scene_server::window::Compositor>>, binders: &[u32], comp: &Mutex<fresco_scene_server::window::Compositor>) -> std::io::Result<()> {
    use aqueduct::envelope::flag;
    use fresco_protocol::activation::{op, Consume, Grant, RaiseResult};
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
                // A consumer spends only its own purposes; the refusal
                // spends nothing.
                Ok(c) if may_consume.as_ref().is_some_and(|m| may_spend(m, c.purpose)) || self_spend(uid, &c) => {
                    let now = now_ns();
                    let r = table().lock().unwrap().consume(now, &c.token, c.presenter_uid, c.purpose, focused, |w, minted| crate::credential::visible_for_purpose(c.purpose, w, minted, now));
                    // A link press spent: one raise of the app it goes to.
                    // A notification action spent (tabula.md §5.5): one too,
                    // for praecod to raise the app's window when it asks.
                    if matches!(c.purpose, purpose::LINK | purpose::ACTION) && matches!(r, ConsumeResult::Ok { .. }) {
                        raises().lock().unwrap().add(uid, now);
                    }
                    // A paste or drop spent: one strip note, should the
                    // broker refuse it after all (personas.md §6).
                    if let (purpose::PASTE | purpose::DROP, ConsumeResult::Ok { window_id, .. }) = (c.purpose, r) {
                        notes().lock().unwrap().owe(uid, window_id, now);
                    }
                    r
                }
                Ok(_) => refused(refusal::NOT_ALLOWED),
                Err(_) => refused(refusal::UNKNOWN),
            }),
            op::GRANT => fresco_protocol::encode(&match fresco_protocol::decode::<Grant>(&msg.payload) {
                Ok(g) if may_grant => table().lock().unwrap().grant(now_ns(), &g.token, uid, g.for_uid, g.purpose),
                Ok(_) => GrantResult::Refused { reason: refusal::NOT_ALLOWED },
                Err(_) => GrantResult::Refused { reason: refusal::UNKNOWN },
            }),
            op::RAISE => fresco_protocol::encode(&match fresco_protocol::decode::<fresco_protocol::activation::Raise>(&msg.payload) {
                Ok(r) if raises().lock().unwrap().take(uid, now_ns()) => raise(comp, r.uid),
                Ok(_) => RaiseResult::Refused { reason: refusal::NOT_ALLOWED },
                Err(_) => RaiseResult::Refused { reason: refusal::UNKNOWN },
            }),
            op::NOTE => fresco_protocol::encode(&match fresco_protocol::decode::<fresco_protocol::activation::Note>(&msg.payload) {
                Ok(n) if notes().lock().unwrap().show(uid, &n, now_ns()) => {
                    eprintln!("frescod: a strip note over window {} for uid {uid}: {}", n.window_id, note_text(&n).unwrap_or_default());
                    note_shown();
                    fresco_protocol::activation::NoteResult::Ok
                }
                Ok(_) => fresco_protocol::activation::NoteResult::Refused { reason: refusal::NOT_ALLOWED },
                Err(_) => fresco_protocol::activation::NoteResult::Refused { reason: refusal::UNKNOWN },
            }),
            fresco_protocol::slot::op::PRESSED => {
                use fresco_protocol::slot::{refusal as no, PressedQuery, PressedResult};
                fresco_protocol::encode(&match (fresco_protocol::decode::<PressedQuery>(&msg.payload), binder) {
                    (Ok(q), Some(comp)) => match comp.lock().unwrap().window_by_token(&q.window) {
                        Some(w) => PressedResult::Ok { pressed: table().lock().unwrap().pressed_within(u32::from(w), now_ns(), q.within_ms) },
                        None => PressedResult::Refused { reason: no::UNKNOWN_WINDOW },
                    },
                    (Ok(_), None) => PressedResult::Refused { reason: no::NOT_ALLOWED },
                    (Err(_), _) => PressedResult::Refused { reason: no::INVALID },
                })
            }
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

    #[test]
    fn a_strip_note_is_owed_once_per_paste_press_and_worded_here() {
        use fresco_protocol::activation::{note, Note};
        let n = |w: u32, p: &str| Note { window_id: w, kind: note::COPIED_IN, persona: p.into() };
        let mut s = Notes::default();
        assert!(!s.show(65039, &n(5, "Work"), 10), "nothing spent");
        s.owe(65039, 5, 10);
        assert!(!s.show(1001, &n(5, "Work"), 11), "another uid's");
        assert!(!s.show(65039, &n(6, "Work"), 11), "another window");
        assert!(!s.show(65039, &Note { kind: 9, ..n(5, "Work") }, 11), "no such note");
        assert!(s.show(65039, &n(5, "Work Stuff!\u{202e}"), 12));
        assert_eq!(s.over(5, 13).as_deref(), Some("paste refused: copied in workstuff"));
        assert_eq!(s.over(6, 13), None, "only over its window");
        assert_eq!(s.over(5, 12 + NOTE_SHOWN_NS), None, "gone");
        assert!(!s.show(65039, &n(5, "Work"), 14), "once");
        s.owe(65039, 5, 20);
        assert!(!s.show(65039, &n(5, "Work"), 20 + NOTE_FOR_NS), "too late");
    }

    #[test]
    fn a_raise_is_owed_once_per_link_press_for_two_minutes() {
        let mut r = Raises::default();
        assert!(!r.take(0, 10), "nothing spent");
        r.add(0, 10);
        assert!(!r.take(1001, 11), "another uid's");
        assert!(r.take(0, 12));
        assert!(!r.take(0, 13), "once");
        r.add(0, 20);
        assert!(!r.take(0, 20 + RAISE_FOR_NS), "too late");
    }

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
    fn consumers_name_their_purposes() {
        let c = parse_consumers("0, 65021=prompt, 70=fill+paste, 71=nonsense, 70=12, x=fill");
        assert_eq!(c.get(&0), Some(&None), "a bare uid: any purpose (tests)");
        assert_eq!(c.get(&65021), Some(&Some(vec![purpose::PROMPT])));
        assert_eq!(c.get(&70), Some(&Some(vec![purpose::FILL, purpose::PASTE, purpose::PROMPT])), "merged");
        assert!(!c.contains_key(&71), "an entry that doesn't parse is dropped");
        assert!(may_spend(&None, purpose::OPEN));
        assert!(may_spend(&Some(vec![purpose::PROMPT]), purpose::PROMPT));
        assert!(!may_spend(&Some(vec![purpose::PROMPT]), purpose::PASTE), "navigatord can't paste");
        let named: Vec<u8> = DEFAULT_CONSUMERS.iter().flat_map(|(_, p)| p.iter().copied()).collect();
        for p in [purpose::COPY, purpose::PASTE, purpose::DROP, purpose::DRAG, purpose::SHARE, purpose::ACTION, purpose::FILL, purpose::IDENTITY, purpose::OPEN, purpose::CONSOLE, purpose::APPROVE, purpose::LINK] {
            assert_eq!(named.iter().filter(|&&q| q == p).count(), 1, "purpose {p}: exactly one default broker");
        }
        for p in SELF_SPENT {
            assert!(!named.contains(p), "purpose {p} is self-spent, no broker's");
        }
    }

    #[test]
    fn a_prompt_press_is_spent_only_by_its_windows_owner() {
        use fresco_protocol::activation::Consume;
        let c = |presenter_uid, purpose| Consume { token: [1; 16], presenter_uid, purpose };
        assert!(self_spend(1001, &c(1001, purpose::PROMPT)));
        assert!(!self_spend(1002, &c(1001, purpose::PROMPT)), "another uid's window");
        assert!(!self_spend(1001, &c(1001, purpose::PASTE)), "not self-spent");
        assert!(!self_spend(1001, &c(1001, purpose::APPROVE)), "not self-spent");
    }

    #[test]
    fn a_prompt_answer_needs_the_windows_guard_seen_whole() {
        let mut t = Table::default();
        let click = t.mint(button(0));
        let guard = |w: u32, _| w == 5 | crate::credential::GUARD;
        assert_eq!(why(t.consume(1, &click, 1001, purpose::PROMPT, YES, |w, _| w == 5)), refusal::OBSCURED, "the window, not its guard");
        assert_eq!(why(t.consume(1, &click, 1002, purpose::PROMPT, YES, guard)), refusal::WRONG_UID);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::PROMPT, |_| false, guard)), refusal::NOT_FOCUSED);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::PROMPT, YES, guard)), 0);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::PROMPT, YES, guard)), refusal::UNKNOWN, "spent");
        let enter = t.mint(key(HID_ENTER, 0, 0));
        assert_eq!(why(t.consume(1, &enter, 1001, purpose::PROMPT, YES, guard)), 0);
        let typed = t.mint(key(0x16, 0, 0));
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::PROMPT, YES, guard)), refusal::WRONG_KIND);
    }

    #[test]
    fn an_identity_press_is_a_click_or_enter_on_a_sheet_seen_whole() {
        let mut t = Table::default();
        let click = t.mint(button(0));
        assert_eq!(why(t.consume(1, &click, 1001, purpose::IDENTITY, YES, NOVIS)), refusal::OBSCURED);
        assert_eq!(why(t.consume(1, &click, 1002, purpose::IDENTITY, YES, |_, _| true)), refusal::WRONG_UID);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::IDENTITY, YES, |_, _| true)), 0);
        let enter = t.mint(key(HID_ENTER, 0, 0));
        assert_eq!(why(t.consume(1, &enter, 1001, purpose::IDENTITY, |_| false, |_, _| true)), refusal::NOT_FOCUSED);
        let typed = t.mint(key(0x1e, 0, 0));
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::IDENTITY, YES, |_, _| true)), refusal::WRONG_KIND, "a PIN digit authorises nothing");
    }

    #[test]
    fn a_console_is_terminal_pressed_in_the_launchers_window() {
        let mut t = Table::default();
        let click = t.mint(button(0));
        assert_eq!(why(t.consume(1, &click, 1002, purpose::CONSOLE, YES, NOVIS)), refusal::WRONG_UID, "another app's press");
        assert_eq!(why(t.consume(1, &click, 1001, purpose::CONSOLE, YES, NOVIS)), 0);
        let enter = t.mint(key(HID_ENTER, 0, 0));
        assert_eq!(why(t.consume(1, &enter, 1001, purpose::CONSOLE, YES, NOVIS)), 0);
        let typed = t.mint(key(0x04, 0, 0));
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::CONSOLE, YES, NOVIS)), refusal::WRONG_KIND, "typing opens no console");
        let tab = t.mint(key(HID_T, MOD_CTRL | MOD_SHIFT, 0));
        assert_eq!(why(t.consume(1, &tab, 1001, purpose::CONSOLE, YES, NOVIS)), 0, "Ctrl+Shift+T in the terminal");
        let plain_t = t.mint(key(HID_T, MOD_CTRL, 0));
        assert_eq!(why(t.consume(1, &plain_t, 1001, purpose::CONSOLE, YES, NOVIS)), refusal::WRONG_KIND, "Ctrl+T is the shell's");
    }

    #[test]
    fn allowing_an_app_is_allow_pressed_in_the_settings_window_seen_whole() {
        let mut t = Table::default();
        let vis = |_: u32, _: u64| true;
        let click = t.mint(button(0));
        assert_eq!(why(t.consume(1, &click, 1002, purpose::APPROVE, YES, vis)), refusal::WRONG_UID, "another app's press");
        assert_eq!(why(t.consume(1, &click, 1001, purpose::APPROVE, YES, NOVIS)), refusal::OBSCURED, "covered, or just moved");
        assert_eq!(why(t.consume(1, &click, 1001, purpose::APPROVE, |_| false, vis)), refusal::NOT_FOCUSED);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::APPROVE, YES, vis)), 0);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::APPROVE, YES, vis)), refusal::UNKNOWN, "spent once");
        let enter = t.mint(key(HID_ENTER, 0, 0));
        assert_eq!(why(t.consume(1, &enter, 1001, purpose::APPROVE, YES, vis)), 0);
        let typed = t.mint(key(0x04, MOD_CTRL, 0));
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::APPROVE, YES, vis)), refusal::WRONG_KIND, "a shortcut allows nothing");
    }

    #[test]
    fn an_open_is_a_click_or_enter_in_the_file_managers_window() {
        let mut t = Table::default();
        let click = t.mint(button(0));
        assert_eq!(why(t.consume(1, &click, 1002, purpose::OPEN, YES, NOVIS)), refusal::WRONG_UID, "another app's press");
        assert_eq!(why(t.consume(1, &click, 1001, purpose::OPEN, |_| false, NOVIS)), refusal::NOT_FOCUSED);
        assert_eq!(why(t.consume(1, &click, 1001, purpose::OPEN, YES, NOVIS)), 0);
        let enter = t.mint(key(HID_ENTER, 0, 0));
        assert_eq!(why(t.consume(1, &enter, 1001, purpose::OPEN, YES, NOVIS)), 0);
        let typed = t.mint(key(0x04, 0, 0));
        assert_eq!(why(t.consume(1, &typed, 1001, purpose::OPEN, YES, NOVIS)), refusal::WRONG_KIND, "typing opens nothing");
        let paste = t.mint(key(HID_V, MOD_CTRL, 0));
        assert_eq!(why(t.consume(1, &paste, 1001, purpose::OPEN, YES, NOVIS)), refusal::WRONG_KIND);
        let forum = 900;
        let press = t.mint(Entry { uid: forum, ..button(0) });
        assert_eq!(granted(t.grant(1, &press, forum, 1001, purpose::OPEN)), Err(refusal::WRONG_KIND), "Forum can't grant an open");
    }

    #[test]
    fn a_press_is_seen_on_its_own_window_for_as_long_as_asked() {
        let mut t = Table::default();
        let s = 1_000_000_000u64;
        t.pressed_at(7, 10 * s);
        assert!(t.pressed_within(7, 10 * s + 900_000_000, 1000));
        assert!(!t.pressed_within(7, 11 * s + 1, 1000), "a second and a bit later");
        assert!(!t.pressed_within(8, 10 * s, 1000), "another window");
        assert!(!t.pressed_within(7, 16 * s, 60_000), "never past PRESSED_MAX_MS");
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
