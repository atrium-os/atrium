//! The Navigator's control plane (navigator-ui.md §2; backend §3).
//!
//! Two sockets, both navigatord's:
//!
//! - the **control socket**, where the UI asks for pages by address and
//!   for link decisions, and is told what was opened. The UI never gets a
//!   page's bytes: an opened page comes back as a one-shot ticket the UI
//!   passes to its doc-viewer child (Limen L6), which redeems it here;
//! - the **page socket**, where a doc-viewer child redeems a ticket for the
//!   recording and the address it was converted from.
//!
//! Frames are a little-endian `u32` length and a postcard body, at most
//! [`MAX_FRAME`]. A descriptor rides alone right after the frame that
//! announces it (`Chrome`), so a reader never buffers past a frame.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

/// The largest control frame. Addresses are bounded by [`URL_MAX`].
pub const MAX_FRAME: usize = 64 * 1024;
/// The longest address either side sends.
pub const URL_MAX: usize = 16 * 1024;
/// The longest title navigatord reports.
pub const TITLE_MAX: usize = 256;
/// The largest recording a ticket redeems for.
pub const RECORDING_MAX: u64 = 128 << 20;
/// The most images a ticket redeems for with its page, and their encoded
/// bytes in all: the converter's verified blobs, by content address.
pub const IMAGES_MAX: usize = 256;
pub const IMAGE_BYTES_MAX: u64 = 32 << 20;
/// An image's content address as the recording names it: `blake3:` and
/// 64 lowercase hex digits.
pub fn is_image_address(a: &str) -> bool {
    a.strip_prefix("blake3:").is_some_and(is_address)
}

pub type Ticket = [u8; 16];

/// navigatord's page socket, as the `navigator-page` capability mounts it:
/// one per navigatord instance (a person's, or a persona's: personas.md §3),
/// all in the one shared directory, named by the instance's uid.
pub const PAGE_SOCKET: &str = "/atrium/sockets/navigator-page/{instance}.sock";

/// A fresh ticket from the navigatord running as `instance` (its uid): the
/// uid first, so the page child knows whose socket redeems it, then 96
/// random bits, which are what make it unguessable.
pub fn ticket_for(instance: u32, random: [u8; 12]) -> Ticket {
    let mut t = [0u8; 16];
    t[..4].copy_from_slice(&instance.to_le_bytes());
    t[4..].copy_from_slice(&random);
    t
}

/// The instance a ticket names.
pub fn ticket_instance(t: &Ticket) -> u32 {
    u32::from_le_bytes([t[0], t[1], t[2], t[3]])
}

/// A socket path with `{instance}` filled in (a path without it is taken
/// as it is: a test's own).
pub fn instance_path(path: &str, instance: u32) -> String {
    path.replace("{instance}", &instance.to_string())
}

/// The most page records one `Clear` names.
pub const CLEAR_MAX: usize = 512;
/// The largest trail store navigatord keeps for the UI (navigator-ui.md
/// §2.3a J3), sent in [`TRAILS_PART`]-byte parts.
pub const TRAILS_MAX: usize = 4 << 20;
/// The most of the trail store one frame carries.
pub const TRAILS_PART: usize = 48 * 1024;
/// The most settings navigatord passes a UI it started.
pub const SETTINGS_MAX: usize = 32;

/// `text` in parts of at most [`TRAILS_PART`] bytes, split between
/// characters; at least one part.
pub fn parts(text: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut rest = text;
    while rest.len() > TRAILS_PART {
        let mut cut = TRAILS_PART;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

/// UI → navigatord.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The first message. `chrome`: send a frescod connection opened over
    /// the chrome channel (F4d) for the UI's window, so navigatord can name
    /// the page's origin in frescod's strip. `limen`: send a limend
    /// connection opened for the UI (navigator-ui.md §2.3a), answered by
    /// `Limen` after `Chrome`.
    /// `measure`: a second frescod connection, for measuring text, answered
    /// by `Measure`; `trails`: the trail store navigatord keeps for the UI,
    /// answered by `Trails` parts (navigator-ui.md §2.3a J3).
    Hello { chrome: bool, limen: bool, measure: bool, trails: bool },
    /// The UI's trail store, to keep: in [`parts`], the last with `last`;
    /// at most [`TRAILS_MAX`] in all.
    SaveTrails { part: String, last: bool },
    /// The UI's window on that connection (its local id), whose origin
    /// navigatord sets.
    Window { window: u32 },
    /// Fetch and convert the page at `url` (or take it from the cache) and
    /// show its origin. Answered by `Opened` or `Failed` with the same `id`.
    /// `saved`: the address an earlier `Opened` gave for this page (a
    /// parked page, navigator-ui.md §4.4): navigatord reopens that version
    /// from its store if it holds it and it is the page at `url`, else it
    /// fetches as usual.
    /// `jar`: the persona's jar the page is in (§2.4); an Unrecorded
    /// trail's pages are kept in memory only, never written. `knows`: the
    /// person let this site remember them in that jar (§6.2), so its
    /// requests carry the jar's cookies (through Arca) and its conversion
    /// is private.
    /// `fresh`: the current version (Reload, "Fetch the current version",
    /// §5.5): fetched again, neither the cache nor `saved` taken.
    Open { id: u32, url: String, saved: Option<String>, jar: Jar, knows: bool, fresh: bool },
    /// Clearing history (§5.5): take these page records out of the store,
    /// with each recording and image no record left still names, and drop
    /// the pages they were from the cache. Only page records navigatord
    /// wrote are taken; anything else named is ignored. At most
    /// [`CLEAR_MAX`] a frame: the UI sends more frames for more.
    Clear { records: Vec<String> },
    /// Forget: with `url`, its site's cookies in the jar ("Forget this
    /// site"); without, an Unrecorded trail's whole jar (the trail closed).
    Forget { jar: Jar, url: Option<String> },
    /// "Sign out here" (§6.2): end the session cookies of `url`'s site in
    /// the jar; cookies with an expiry, and the site knowing you, stay.
    /// Answered by `SignedOut`.
    SignOut { jar: Jar, url: String },
    /// The site's last page in the jar went (its trails put away, its pages
    /// cleared) while it still knows you: its session cookies end (Arca
    /// §8.6), as a browser's do when it closes. No answer.
    EndSession { jar: Jar, url: String },
    /// The presence menu opened on a page whose site knows you: which
    /// accounts Arca holds for `url`'s site (names only). Answered by
    /// `Accounts`.
    Accounts { url: String },
    /// The UI shows a page it already has live (a page child it kept):
    /// name its origin in the strip. Only a page navigatord opened for
    /// this UI is named; anything else blanks the strip.
    Show { url: String },
    /// What activating `href` in the page at `page` does. Answered by
    /// `Link` with the same `id`. `jar`: the persona's jar the page is in,
    /// where a consent control's press keeps its cookies (§6.2).
    Activate { id: u32, page: String, href: String, jar: Jar },
    /// The UI shows no page (Start, a stop page): no origin in the strip.
    Blank,
    /// The person pressed Save on a file an `Open` turned out to be
    /// (`File`, same `id`, navigator-ui.md §6.4). navigatord writes it to
    /// the downloads directory with its source origin recorded, never
    /// executable; answered by `Saved` or `NotSaved`.
    /// `press`: frescod's activation token for the press on Save, which
    /// navigatord spends with frescod (`purpose::PROMPT`) before acting:
    /// the prompt row must have been whole, uncovered by any other window
    /// and still for 500 ms before it.
    Save { id: u32, press: Press },
    /// The person cancelled that file: navigatord drops the bytes.
    Discard { id: u32 },
    /// The person pressed a hand-off prompt's answer for a link navigatord
    /// decided as `Handoff` on this connection (threat model §2.10). Only
    /// such a link is handed on; answered by `Handed`. `press` as for `Save`.
    Hand { url: String, how: Hand, press: Press },
    /// The rail's New window menu (navigator-ui.md §2.4): a Navigator
    /// window in another of the person's personas (`personal` for
    /// Personal), one navigatord passed in `NAVIGATOR_UI_OTHER_PERSONAS`.
    /// navigatord asks portcullisd (`NewWindowIn`) with the press, which
    /// starts that persona's Navigator or brings it forward. Nothing of
    /// this window goes with it. Answered by `WindowIn`.
    NewWindow { persona: String, press: Press },
    /// The person pressed Open on a held file (§6.4): navigatord spends the
    /// press as for `Save`, then hands the file to portcullisd, which opens
    /// it read-only in the person's default app for its type, in this
    /// window's persona; nothing is written. Answered by `FileOpened`.
    OpenFile { id: u32, press: Press },
    /// Send a form of the page at `page` (navigator-ui.md "Forms in the
    /// sign-in window"; atrium-net.md §8.1 P5), on the person's press on
    /// one of its buttons in the window's form panel. Only the sign-in
    /// window's navigatord sends one. `action` as the page spells it (empty:
    /// the page's own address); `fields` in order, each typed by the person
    /// or as the page gave it, which navigatord checks the page carries.
    /// Answered by `Link` with the same `id`: `Navigate` to the answer.
    SubmitForm { id: u32, page: String, jar: Jar, action: String, post: bool, fields: Vec<FormField>, press: Press },
    RunLive { url: String, jar: Jar, press: Press },
}

/// The single whole-page fixture granted during I0.
pub const INBOX_FIXTURE_URL: &str = "https://inbox.example/inbox";

/// One field of a form sent: its name and value, and whether the person
/// typed the value (else it is the page's, and navigatord checks it is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    pub value: String,
    pub typed: bool,
    /// A password typed into Arca's field (a VPN sign-in window's): Arca's
    /// handle, and `value` is empty. The value never reaches the UI or
    /// navigatord; Arca's courier puts it in the form (arca-autofill.md §3).
    pub arca: Option<[u8; 16]>,
}

/// The most fields a form sent has, and the longest name or value.
pub const FORM_FIELDS_MAX: usize = 32;
pub const FORM_TEXT_MAX: usize = 1024;
/// The longest value a form sends as its page gave it (a sign-in's hidden
/// assertion; limen's `FIXED_VALUE_MAX`).
pub const FORM_FIXED_MAX: usize = 32 * 1024;

/// A `SubmitForm` in shape.
pub fn form_ok(action: &str, fields: &[FormField]) -> bool {
    clean(action, URL_MAX) && fields.len() <= FORM_FIELDS_MAX
        && fields.iter().all(|f| !f.name.is_empty() && clean(&f.name, FORM_TEXT_MAX) && clean(&f.value, if f.typed { FORM_TEXT_MAX } else { FORM_FIXED_MAX })
            && (f.arca.is_none() || (f.typed && f.value.is_empty())))
        && fields.iter().filter(|f| f.arca.is_some()).count() <= 1
}

/// frescod's activation token for a press (zero: none).
pub type Press = [u8; 16];

/// How a hand-off is taken (navigator-ui.md §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hand {
    /// To the scheme's handler (`mailto:` to Mail; an app installed).
    Open,
    /// An `atrium-app:` link in an ephemeral jail that keeps nothing
    /// (insula trial launch).
    Try,
    /// The page's own address, "Open with…": Monstrator's sheet always
    /// asks, for another app or another persona (personas.md §6). Only
    /// for a page navigatord holds.
    With,
}

/// Files bigger than this are not kept for saving (navigatord holds them in
/// memory until the person answers).
pub const FILE_MAX: u64 = 64 << 20;
/// The longest file name navigatord reports or writes.
pub const NAME_MAX: usize = 128;

/// What the open-an-app prompt shows of an `atrium-app:` link's app
/// (navigator-ui.md §6.4), from opifexd's `Describe`: who publishes it and
/// what it will ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppAbout {
    /// Its name in the catalog.
    pub name: String,
    /// The publisher's name and the domains it proved, or "Atrium".
    pub publisher: String,
    /// The capabilities its newest release asks for.
    pub capabilities: Vec<String>,
    /// Installed: Open starts it; otherwise Open asks to install it first.
    pub installed: bool,
    /// From a catalog someone added, not the Atrium catalog.
    pub third_party: bool,
}

/// What a link does (backend `links::LinkAction`, and refusals).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkAction {
    Navigate { url: String, insecure: bool },
    InDocument { fragment: String },
    /// `app`: for an `atrium-app:` link, what the package catalog says of
    /// the app (navigator-ui.md §6.4), when it knows it.
    Handoff { url: String, scheme: String, app: Option<AppAbout> },
    Blocked { why: String },
    /// A consent wall's control (`atrium-consent:<n>`, legacy-web
    /// §5.4.1e): the reader's press kept what it writes in the page's
    /// jar, so the site now knows you there; the UI opens `url` again
    /// with the site knowing you.
    Consented { url: String },
}

/// navigatord → UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The answer to `Hello`. With `fd`, a descriptor follows: the UI's
    /// frescod connection.
    Chrome { fd: bool },
    /// After `Chrome`, when `Hello` asked for it. With `fd`, a descriptor
    /// follows: a limend connection navigatord opened.
    Limen { fd: bool },
    /// After `Limen`, when `Hello` asked. With `fd`, a descriptor follows: a
    /// second frescod connection, for measuring text.
    Measure { fd: bool },
    /// After `Measure`, when `Hello` asked: the kept trail store in parts,
    /// the last with `last`; `None` in every part when there is none.
    Trails { part: Option<String>, last: bool },
    /// The first frame to a UI navigatord started as its worker (its
    /// channel is its stdin, and a jail has no environment of the
    /// caller's): its `NAVIGATOR_UI_*` settings.
    Settings { pairs: Vec<(String, String)> },
    /// The answer to `SaveTrails`' last part when it could not be kept.
    TrailsNotSaved { why: String },
    /// `saved`: the store address of this version of the page, for the
    /// UI's trail record; `from`: where it came from.
    /// `knows`: whether it was fetched with the site knowing you (false
    /// if asked but Arca could not carry it; `note` says why).
    /// `asked`: the site asked to know you (§6.2): its cookie check failed
    /// without cookies, so navigatord opened it with them; the UI shows it.
    Opened { id: u32, url: String, title: String, insecure: bool, ticket: Ticket, ms: u32, saved: String, from: Source, knows: bool, asked: bool, note: Option<String> },
    Failed { id: u32, url: String, why: String },
    Link { id: u32, action: LinkAction },
    /// The page an `Open` asked for is a file, not a document: navigatord
    /// keeps it until `Save` or `Discard` (and drops it unanswered after
    /// a while). `name` is a safe file name, `origin` the site it came
    /// from, `kind` what its bytes look like ("PDF", "ZIP archive", …).
    File { id: u32, url: String, name: String, size: u64, origin: String, kind: String },
    /// Written: `path` is where.
    Saved { id: u32, path: String },
    NotSaved { id: u32, why: String },
    /// frescod didn't count the press that came with `Save` or `Hand`
    /// (something was over the prompt row, or it had just moved): nothing
    /// was done, and the file or hand-off is still held.
    PressRefused { why: String },
    /// A link another app opened in the Navigator (opening-links.md §5),
    /// as navigatord took it from its document channel: a web address.
    /// The UI opens it in a trail of its own. `from`: the person sent it
    /// here from another of their personas (its name, `personal` for
    /// Personal); a named persona's window opens it anonymous
    /// (personas.md §6).
    Incoming { url: String, from: Option<String> },
    /// A hand-off was passed to its handler, or why not.
    Handed { url: String, ok: bool, why: Option<String> },
    /// `NewWindow` done, or why not.
    WindowIn { persona: String, ok: bool, why: Option<String> },
    /// `SignOut` done: how many session cookies ended, or why not.
    SignedOut { url: String, cookies: Option<u32>, why: Option<String> },
    /// The accounts Arca holds for `url`'s site, at most [`ACCOUNTS_MAX`];
    /// `why` when Arca couldn't be asked.
    Accounts { url: String, accounts: Vec<Account>, why: Option<String> },
    /// `OpenFile` done: the app it opened in, or why not (the file is
    /// dropped either way).
    FileOpened { id: u32, app: Option<String>, why: Option<String> },
    LivePageFailed { why: String },
}

/// The most accounts an `Accounts` names, and the longest name.
pub const ACCOUNTS_MAX: usize = 16;
pub const ACCOUNT_NAME_MAX: usize = 128;

/// An account Arca holds for a site (arcad `W_ACCOUNTS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub kind: AccountKind,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountKind {
    Passkey,
    Password,
}

/// A persona's jar (navigator-ui.md §2.4): Personal, or one Unrecorded
/// trail's own throwaway jar, named by a random id the UI picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Jar {
    Personal,
    Unrecorded(u64),
}

impl Jar {
    pub fn unrecorded(&self) -> bool { matches!(self, Jar::Unrecorded(_)) }
}

/// Where an opened page came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Fetched and converted now.
    Fetched,
    /// navigatord's in-memory cache of recent pages.
    Cache,
    /// navigatord's store, by the saved address (a parked page).
    Store,
}

/// A store address as navigatord gives them: 64 lowercase hex digits
/// (BLAKE3).
pub fn is_address(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a ticket redeems for on the page socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHeader {
    /// The address the document was converted from (links resolve against it).
    pub base: String,
    /// The recording's length; its bytes follow the frame.
    pub len: u64,
    /// The page's images (content address, encoded length), their bytes
    /// following the recording's in this order. Still hostile: the child
    /// checks each against its address and decodes it in its own jail.
    pub images: Vec<(String, u64)>,
}

/// The images a page carries, bounded: in count, in bytes, and each named
/// by a content address.
pub fn images_ok(images: &[(String, u64)]) -> bool {
    images.len() <= IMAGES_MAX
        && images.iter().map(|(_, n)| *n).try_fold(0u64, |t, n| t.checked_add(n)).is_some_and(|t| t <= IMAGE_BYTES_MAX)
        && images.iter().all(|(a, _)| is_image_address(a))
}

fn bad(m: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, m.into()) }

/// Write one frame.
pub fn write_frame<T: Serialize>(w: &mut impl Write, m: &T) -> io::Result<()> {
    let b = postcard::to_stdvec(m).map_err(|e| bad(e.to_string()))?;
    if b.len() > MAX_FRAME {
        return Err(bad("frame over MAX_FRAME"));
    }
    let mut out = (b.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&b);
    w.write_all(&out)
}

/// Read one frame; `Ok(None)` at a clean end of stream.
pub fn read_frame<T: for<'a> Deserialize<'a>>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(bad(format!("a {n}-byte frame")));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    postcard::from_bytes(&b).map(Some).map_err(|e| bad(e.to_string()))
}

/// Send one descriptor alone (after the frame that announced it).
pub fn send_fd(s: &UnixStream, fd: RawFd) -> io::Result<()> {
    portcullis_ipc::fdpass::send_fds(s, &[fd])
}

/// Receive the descriptor a frame announced.
pub fn recv_fd(s: &UnixStream) -> io::Result<OwnedFd> {
    portcullis_ipc::fdpass::recv_fds(s, 1)?.pop().ok_or_else(|| bad("no descriptor"))
}

/// A string fit to send: no controls, at most `max` bytes.
pub fn clean(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

/// What a ticket redeems for: the base address, the recording and the
/// page's images (address, encoded bytes).
pub struct Redeemed {
    pub base: String,
    pub recording: Vec<u8>,
    pub images: Vec<(String, Vec<u8>)>,
}

/// The child's side of the page socket: redeem `ticket` for the base
/// address, the recording (at most `RECORDING_MAX` bytes) and its images
/// (within `images_ok`).
pub fn redeem(path: &str, ticket: &Ticket) -> io::Result<Redeemed> {
    let mut s = UnixStream::connect(path)?;
    s.write_all(ticket)?;
    read_page(&mut s)
}

/// Read what the page socket sends after a ticket.
pub fn read_page(s: &mut impl Read) -> io::Result<Redeemed> {
    let h: PageHeader = read_frame(s)?.ok_or_else(|| bad("the ticket was refused"))?;
    if h.len > RECORDING_MAX || !clean(&h.base, URL_MAX) || !images_ok(&h.images) {
        return Err(bad("a header out of bounds"));
    }
    let mut exact = |n: u64, what: &str| -> io::Result<Vec<u8>> {
        let mut b = Vec::with_capacity(n as usize);
        s.take(n).read_to_end(&mut b)?;
        if b.len() as u64 != n {
            return Err(bad(format!("a short {what}")));
        }
        Ok(b)
    };
    let recording = exact(h.len, "recording")?;
    let mut images = Vec::with_capacity(h.images.len());
    for (a, n) in h.images {
        let b = exact(n, "image")?;
        images.push((a, b));
    }
    Ok(Redeemed { base: h.base, recording, images })
}

/// The page socket's side: the header, the recording and the images.
pub fn write_page(w: &mut impl Write, base: &str, recording: &[u8], images: &[(String, Vec<u8>)]) -> io::Result<()> {
    let h = PageHeader { base: base.into(), len: recording.len() as u64, images: images.iter().map(|(a, b)| (a.clone(), b.len() as u64)).collect() };
    write_frame(w, &h)?;
    w.write_all(recording)?;
    for (_, b) in images {
        w.write_all(b)?;
    }
    Ok(())
}

/// The raw descriptor of a stream, for `send_fd`.
pub fn raw(s: &impl AsRawFd) -> RawFd { s.as_raw_fd() }

#[cfg(test)]
mod tests {
    #[test]
    fn an_arca_field_sends_a_handle_never_a_value() {
        let f = |v: &str, typed, arca| FormField { name: "password".into(), value: v.into(), typed, arca };
        assert!(form_ok("/login", &[f("", true, Some([1; 16]))]));
        assert!(!form_ok("/login", &[f("pw", true, Some([1; 16]))]), "a handle and a value");
        assert!(!form_ok("/login", &[f("", false, Some([1; 16]))]), "the page's value can't be Arca's");
        assert!(!form_ok("/login", &[f("", true, Some([1; 16])), f("", true, Some([2; 16]))]), "one per form");
    }


    #[test]
    fn a_ticket_names_the_instance_whose_socket_redeems_it() {
        let t = ticket_for(70012, [9; 12]);
        assert_eq!(ticket_instance(&t), 70012);
        assert_eq!(&t[4..], &[9; 12]);
        assert_eq!(instance_path(PAGE_SOCKET, ticket_instance(&t)), "/atrium/sockets/navigator-page/70012.sock");
        assert_eq!(instance_path("/tmp/p.sock", 70012), "/tmp/p.sock");
    }

    use super::*;

    #[test]
    fn a_page_comes_back_with_its_images_and_bounds_hold() {
        let a = format!("blake3:{}", "ab".repeat(32));
        let mut buf = vec![];
        write_page(&mut buf, "https://a.example/", b"rec", &[(a.clone(), vec![1, 2, 3])]).unwrap();
        let r = read_page(&mut buf.as_slice()).unwrap();
        assert_eq!((r.base.as_str(), r.recording.as_slice()), ("https://a.example/", &b"rec"[..]));
        assert_eq!(r.images, vec![(a.clone(), vec![1, 2, 3])]);
        // Short image bytes, a bad address, too many bytes: refused.
        assert!(read_page(&mut &buf[..buf.len() - 1]).is_err());
        let mut bad_addr = vec![];
        write_page(&mut bad_addr, "https://a.example/", b"rec", &[("../x".into(), vec![1])]).unwrap();
        assert!(read_page(&mut bad_addr.as_slice()).is_err());
        assert!(!images_ok(&[(a.clone(), IMAGE_BYTES_MAX), (a, 1)]));
    }

    #[test]
    fn frames_round_trip_and_bounds_hold() {
        let mut buf = Vec::new();
        let m = Request::Open { id: 7, url: "https://atlas.example/".into(), saved: Some("ab".repeat(32)), jar: Jar::Unrecorded(7), knows: true, fresh: true };
        let clear = Request::Clear { records: vec!["ab".repeat(32); CLEAR_MAX] };
        let mut big = vec![];
        write_frame(&mut big, &clear).unwrap();
        assert!(big.len() < MAX_FRAME, "a full Clear fits a frame");
        assert!(is_address(&"ab".repeat(32)) && !is_address("AB") && !is_address(&"g".repeat(64)));
        write_frame(&mut buf, &m).unwrap();
        write_frame(&mut buf, &Event::Link { id: 7, action: LinkAction::InDocument { fragment: "a".into() } }).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame::<Request>(&mut r).unwrap(), Some(m));
        assert!(matches!(read_frame::<Event>(&mut r).unwrap(), Some(Event::Link { id: 7, .. })));
        assert_eq!(read_frame::<Event>(&mut r).unwrap(), None);
        let huge = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(read_frame::<Event>(&mut &huge[..]).is_err());
    }

    #[test]
    fn a_descriptor_follows_its_frame() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, _y) = UnixStream::pair().unwrap();
        write_frame(&mut &a, &Event::Chrome { fd: true }).unwrap();
        send_fd(&a, raw(&x)).unwrap();
        assert_eq!(read_frame::<Event>(&mut &b).unwrap(), Some(Event::Chrome { fd: true }));
        recv_fd(&b).unwrap();
    }
}

#[cfg(test)]
mod parts_tests {
    use super::*;

    #[test]
    fn parts_split_between_characters() {
        assert_eq!(parts(""), vec![""]);
        let t = "é".repeat(TRAILS_PART);
        let p = parts(&t);
        assert!(p.iter().all(|x| x.len() <= TRAILS_PART));
        assert_eq!(p.concat(), t);
        let mut buf = vec![];
        write_frame(&mut buf, &Event::Trails { part: Some(p[0].to_string()), last: false }).unwrap();
        write_frame(&mut buf, &Request::SaveTrails { part: p[0].to_string(), last: true }).unwrap();
    }
}
