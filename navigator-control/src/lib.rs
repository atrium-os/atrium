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

/// The most page records one `Clear` names.
pub const CLEAR_MAX: usize = 512;

/// UI → navigatord.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The first message. `chrome`: send a frescod connection opened over
    /// the chrome channel (F4d) for the UI's window, so navigatord can name
    /// the page's origin in frescod's strip.
    Hello { chrome: bool },
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
    /// The presence menu opened on a page whose site knows you: which
    /// accounts Arca holds for `url`'s site (names only). Answered by
    /// `Accounts`.
    Accounts { url: String },
    /// The UI shows a page it already has live (a page child it kept):
    /// name its origin in the strip. Only a page navigatord opened for
    /// this UI is named; anything else blanks the strip.
    Show { url: String },
    /// What activating `href` in the page at `page` does. Answered by
    /// `Link` with the same `id`.
    Activate { id: u32, page: String, href: String },
    /// The UI shows no page (Start, a stop page): no origin in the strip.
    Blank,
    /// The person pressed Save on a file an `Open` turned out to be
    /// (`File`, same `id`, navigator-ui.md §6.4). navigatord writes it to
    /// the downloads directory with its source origin recorded, never
    /// executable; answered by `Saved` or `NotSaved`.
    Save { id: u32 },
    /// The person cancelled that file: navigatord drops the bytes.
    Discard { id: u32 },
    /// The person pressed a hand-off prompt's answer for a link navigatord
    /// decided as `Handoff` on this connection (threat model §2.10). Only
    /// such a link is handed on; answered by `Handed`.
    Hand { url: String, how: Hand },
}

/// How a hand-off is taken (navigator-ui.md §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hand {
    /// To the scheme's handler (`mailto:` to Mail; an app installed).
    Open,
    /// An `atrium-app:` link in an ephemeral jail that keeps nothing
    /// (insula trial launch).
    Try,
}

/// Files bigger than this are not kept for saving (navigatord holds them in
/// memory until the person answers).
pub const FILE_MAX: u64 = 64 << 20;
/// The longest file name navigatord reports or writes.
pub const NAME_MAX: usize = 128;

/// What a link does (backend `links::LinkAction`, and refusals).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkAction {
    Navigate { url: String, insecure: bool },
    InDocument { fragment: String },
    Handoff { url: String, scheme: String },
    Blocked { why: String },
}

/// navigatord → UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// The answer to `Hello`. With `fd`, a descriptor follows: the UI's
    /// frescod connection.
    Chrome { fd: bool },
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
    /// A hand-off was passed to its handler, or why not.
    Handed { url: String, ok: bool, why: Option<String> },
    /// `SignOut` done: how many session cookies ended, or why not.
    SignedOut { url: String, cookies: Option<u32>, why: Option<String> },
    /// The accounts Arca holds for `url`'s site, at most [`ACCOUNTS_MAX`];
    /// `why` when Arca couldn't be asked.
    Accounts { url: String, accounts: Vec<Account>, why: Option<String> },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
