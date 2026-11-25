//! Reports from a broker-fetched web island (navigator-islands.md §5).
use serde::{Deserialize, Serialize};

/// The longest report: a copy's text and framing (more than a submitted
/// form's address and body).
pub const MAX_FRAME: usize = COPY_MAX + 64;
/// The most text a whole page copies or is pasted at once (I6.4).
pub const COPY_MAX: usize = 16 * 1024;
/// The longest address a whole page's engine reports (I6.2).
pub const URL_MAX: usize = 2048;
/// The longest form a whole page submits (I6.3): a sign-in, not an upload.
pub const FORM_MAX: usize = 4096;

/// A whole-page island's first request (navigator-islands.md I6.1): the
/// engine asks for "its page" by this address, which never resolves
/// (`.invalid`), and navigatord answers with the page the person pressed
/// on, its address as the answer's final URL. The engine never names the
/// page it runs.
pub const LIVE_PAGE: &str = "https://live-page.atrium.invalid/";
/// A whole page's `localStorage` (I6.3): a GET loads its site's store, a
/// POST sends a batch of changes. navigatord keeps it; the engine holds
/// no file.
pub const LIVE_STORAGE: &str = "https://live-page.atrium.invalid/local-storage";
/// A whole page's IndexedDB (I6.7): a GET loads its site's databases, a
/// POST sends one part of a committing transaction's changes, and the last
/// part's answer says whether the commit was kept. navigatord keeps it.
pub const LIVE_IDB: &str = "https://live-page.atrium.invalid/indexed-db";
pub mod idb;
/// A file a whole page's script made and asked to save (I6.4: `<a
/// download>` on a `blob:` or `data:` address): its bytes are POSTed here,
/// with `name` (the suggested name) and `type` (its content type) as
/// query parameters. navigatord holds it and the person decides, as for
/// any download.
pub const LIVE_DOWNLOAD: &str = "https://live-page.atrium.invalid/download";
/// A whole page asked for an API I6 refuses (I6.4): a GET here with
/// `api` set to the [`Api`]'s token. navigatord tells the chip's popover.
pub const LIVE_REFUSED: &str = "https://live-page.atrium.invalid/refused";
/// The script navigatord puts first in a whole page (I6.4): it answers the
/// refused APIs at once and reports each to [`LIVE_REFUSED`], and hands a
/// script-made download to [`LIVE_DOWNLOAD`]. It reports; the engine,
/// which has none of these APIs' backends, is what refuses.
pub const LIVE_REPORTER: &str = "https://live-page.atrium.invalid/reporter.js";

/// The file the person picked for a whole page's [`Report::PickFile`]
/// (I6.4): its bytes are a GET of this address, once, after
/// [`FilePicked::Chosen`].
pub const LIVE_FILE: &str = "https://live-page.atrium.invalid/picked-file";
/// The longest name a [`FilePicked`] carries.
pub const PICKED_NAME_MAX: usize = 255;

/// The text the person pasted into a whole page (I6.4), a control
/// message: 'V' and UTF-8, at most [`COPY_MAX`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pasted(pub String);

impl Pasted {
    pub fn encode(&self) -> Option<Vec<u8>> {
        (!self.0.is_empty() && self.0.len() <= COPY_MAX).then(|| [b"V".as_slice(), self.0.as_bytes()].concat())
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes.strip_prefix(b"V")?).ok()?;
        (!text.is_empty() && text.len() <= COPY_MAX).then(|| Self(text.to_string()))
    }
}

/// navigatord's answer to [`Report::PickFile`], a control message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilePicked {
    /// The person picked a file named `name` (no folder); its bytes wait
    /// at [`LIVE_FILE`].
    Chosen { name: String },
    /// The person cancelled, or the picker could not open.
    Cancelled,
}

impl FilePicked {
    pub fn encode(&self) -> Option<Vec<u8>> {
        match self {
            Self::Cancelled => Some(vec![b'F', 0]),
            Self::Chosen { name } if Self::name_ok(name) => Some([&[b'F', 1][..], name.as_bytes()].concat()),
            Self::Chosen { .. } => None,
        }
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [b'F', 0] => Some(Self::Cancelled),
            [b'F', 1, name @ ..] => {
                let name = std::str::from_utf8(name).ok()?;
                Self::name_ok(name).then(|| Self::Chosen { name: name.to_string() })
            }
            _ => None,
        }
    }
    fn name_ok(name: &str) -> bool {
        !name.is_empty() && name.len() <= PICKED_NAME_MAX && !name.chars().any(|c| c.is_control() || c == '/')
    }
}

/// A whole page asked to be printed (`window.print()`, Ctrl-P in the page;
/// I6.4): a GET here. The UI prints the trail entry's document-lane
/// rendering through Prelum's sheet.
pub const LIVE_PRINT: &str = "https://live-page.atrium.invalid/print";

/// A cross-origin frame in a whole page (navigator-islands.md D11, I6.5),
/// reported by navigatord's script in the page: a GET of
/// `frame?id=N&url=U&x=X&y=Y&w=W&h=H` (device pixels in the page's
/// viewport) when it appears or moves, `frame?id=N&gone=1` when it goes.
/// The frame runs in its own engine, an island over the page's.
pub const LIVE_FRAME: &str = "https://live-page.atrium.invalid/frame";

/// `postMessage` between a page and its frames' engines (I6.5): a POST
/// here of the message as JSON text, `?to=N&target=O` from the page to its
/// frame `N`, `?target=O` from a frame to its page. navigatord delivers it
/// only to a receiver whose origin is `O` (or any, for `*`), stamped with
/// the sender's real origin.
pub const LIVE_MESSAGE: &str = "https://live-page.atrium.invalid/message";

/// The messages waiting for this engine (I6.5): a GET answers a JSON array
/// of `{"from": N or null, "origin": O, "data": TEXT}`, oldest first.
pub const LIVE_MESSAGES: &str = "https://live-page.atrium.invalid/messages";

/// A web API a whole page asked for that I6 refuses (navigator-islands.md
/// "What the page can reach"), so the popover can say what the page was
/// refused; nothing is refused silently.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Api {
    Notifications,
    Geolocation,
    Camera,
    Microphone,
    Midi,
    Usb,
    Bluetooth,
    Serial,
    Hid,
    ServiceWorker,
    Push,
    BackgroundSync,
    /// A paste the page asked for outside the person's Ctrl-V.
    ClipboardRead,
    /// A new window (`window.open` that is not a navigation).
    Popup,
    /// A file a script made that is over [`DOWNLOAD_MAX`].
    LargeDownload,
}

/// The largest file a page's script may hand over to save (one POST on the
/// fetch channel, Nuntius's body bound).
pub const DOWNLOAD_MAX: usize = 32 * 1024;

/// The memory limit, in MiB, of an engine that runs a part of a page (a
/// widget, or a live page's frame), and of a standby engine: limend asks
/// portcullisd for it, which can only tighten the engine manifest's. A
/// whole live page gets the manifest's own (navigator-islands.md I6.6).
pub const PART_MEMORY_MB: u64 = 256;

const APIS: &[(Api, &str, &str)] = &[
    (Api::Notifications, "notifications", "notifications"),
    (Api::Geolocation, "geolocation", "your location"),
    (Api::Camera, "camera", "the camera"),
    (Api::Microphone, "microphone", "the microphone"),
    (Api::Midi, "midi", "MIDI devices"),
    (Api::Usb, "usb", "USB devices"),
    (Api::Bluetooth, "bluetooth", "Bluetooth devices"),
    (Api::Serial, "serial", "serial ports"),
    (Api::Hid, "hid", "input devices (HID)"),
    (Api::ServiceWorker, "service-worker", "a service worker"),
    (Api::Push, "push", "push messages"),
    (Api::BackgroundSync, "background-sync", "background sync"),
    (Api::ClipboardRead, "clipboard-read", "reading the clipboard without your paste"),
    (Api::Popup, "popup", "a new window"),
    (Api::LargeDownload, "large-download", "a file it made that is too big to save"),
];

impl Api {
    /// The token the reporter sends.
    pub fn token(self) -> &'static str {
        APIS.iter().find(|a| a.0 == self).map_or("", |a| a.1)
    }
    /// The API a reporter's token names; `None` for any other text.
    pub fn from_token(token: &str) -> Option<Self> {
        APIS.iter().find(|a| a.1 == token).map(|a| a.0)
    }
    /// What the popover says the page was refused.
    pub fn label(self) -> &'static str {
        APIS.iter().find(|a| a.0 == self).map_or("", |a| a.2)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EndReason { Closed, Failed }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Report<'a> {
    Started { origin: &'a str },
    Title(&'a str),
    Resize { width: u16, height: u16 },
    Ended(EndReason),
    BeforeUnload { query: u32 },
    MediaReady,
    /// A whole page asked to go to `url` (I6.2): a link, a GET form,
    /// `location`. The engine never follows it; navigatord decides.
    Navigate(&'a str),
    /// The page's own address changed without a new document
    /// (`history.pushState`, I6.2): a trail entry, same engine.
    UrlChanged(&'a str),
    /// A whole page submitted a form to `url` (I6.3): a POST navigation,
    /// `application/x-www-form-urlencoded`. Never sent by the engine;
    /// navigatord sends it through the page's jar and opens the answer.
    Submit { url: &'a str, body: &'a [u8] },
    /// The person copied in the page (Ctrl-C, I6.4): its selected text and
    /// the press's activation token, which the page's parent spends on
    /// Tabula (frescod lets an island's parent spend it on a copy).
    Copy { text: &'a str, token: [u8; 16] },
    /// The person pressed Ctrl-V in the page (I6.4): the parent spends the
    /// token on a paste and answers with [`Pasted`].
    Paste { token: [u8; 16] },
    /// The page asked for a file (`<input type=file>`, I6.4): one file,
    /// read-only. navigatord opens Scrinium's picker over the window and
    /// answers with [`FilePicked`]; the engine never sees a folder.
    PickFile,
    /// A whole page asked for `url` in a new window (`window.open`, a
    /// `target=_blank` link or form, I6.2): the engine opens none and
    /// reports it only on the person's activation; navigatord opens it
    /// beside the page in the trail.
    OpenBeside(&'a str),
}

/// An address an engine may report: plain HTTPS, bounded, printable.
fn reportable(url: &str) -> bool {
    url.len() <= URL_MAX && url.starts_with("https://") && url.bytes().all(|b| b.is_ascii_graphic())
}

impl Report<'_> {
    fn valid(&self) -> bool {
        match self {
            Self::Started { origin } => origin.len() <= 255 && origin.strip_prefix("https://").is_some_and(|host|
                !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))),
            Self::Title(text) => text.len() <= 256 && !text.chars().any(char::is_control),
            Self::Resize { width, height } => (1..=16384).contains(width) && (1..=16384).contains(height),
            Self::Ended(_) | Self::MediaReady | Self::PickFile => true,
            Self::Copy { text, token } => !text.is_empty() && text.len() <= COPY_MAX && *token != [0; 16],
            Self::Paste { token } => *token != [0; 16],
            Self::BeforeUnload { query } => *query != 0,
            Self::Navigate(url) | Self::UrlChanged(url) | Self::OpenBeside(url) => reportable(url),
            Self::Submit { url, body } => reportable(url) && body.len() <= FORM_MAX && body.iter().all(u8::is_ascii_graphic),
        }
    }

    pub fn encode(self) -> Option<Vec<u8>> {
        if !self.valid() { return None; }
        postcard::to_stdvec(&self).ok()
    }
}

#[derive(Clone, Copy, Default)]
pub enum Lifecycle {
    #[default]
    Waiting,
    Running { last_query: u32 },
    AwaitingUnload(u32),
    Ended,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnloadAnswer { pub query: u32, pub leave: bool }

impl UnloadAnswer {
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = vec![b'U'];
        bytes.extend(self.query.to_le_bytes());
        bytes.push(u8::from(self.leave));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let &[b'U', a, b, c, d, flag @ 0..=1] = bytes else { return None };
        Some(Self { query: u32::from_le_bytes([a, b, c, d]), leave: flag == 1 })
    }
}

/// Chapter control carries only a finite time on the native media clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChapterSeek(f64);

impl ChapterSeek {
    pub fn from_link(href: &str) -> Option<Self> {
        let time = href.strip_prefix("#t=")?;
        let (seconds, fraction) = time.split_once('.').unwrap_or((time, "0"));
        if seconds.is_empty() || fraction.is_empty() ||
            !seconds.bytes().chain(fraction.bytes()).all(|c| c.is_ascii_digit()) { return None; }
        Self::new(time.parse().ok()?)
    }
    pub fn new(seconds: f64) -> Option<Self> {
        crate::media_player::finite_secs(seconds).then_some(Self(seconds))
    }
    pub fn seconds(self) -> f64 { self.0 }
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = vec![b'S'];
        bytes.extend(self.0.to_le_bytes());
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        Self::new(f64::from_le_bytes(bytes.strip_prefix(b"S")?.try_into().ok()?))
    }
}

/// The page area a whole page is placed in (I6.2), from its parent: the
/// engine lays the page out at this size, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSize { width: u16, height: u16 }

impl PageSize {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let (width, height) = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
        ((1..=16384).contains(&width) && (1..=16384).contains(&height)).then_some(Self { width, height })
    }
    pub fn width(self) -> u16 { self.width }
    pub fn height(self) -> u16 { self.height }
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = vec![b'P'];
        bytes.extend(self.width.to_le_bytes());
        bytes.extend(self.height.to_le_bytes());
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let &[b'P', a, b, c, d] = bytes else { return None };
        Self::new(u16::from_le_bytes([a, b]).into(), u16::from_le_bytes([c, d]).into())
    }
}

impl Lifecycle {
    pub fn accepts_control(&mut self, bytes: &[u8], media_ready: bool) -> bool {
        if let Some(answer) = UnloadAnswer::decode(bytes) { return self.answer(answer); }
        let running = matches!(self, Self::Running { .. });
        if PageSize::decode(bytes).is_some() || FilePicked::decode(bytes).is_some() || Pasted::decode(bytes).is_some() { return running; }
        media_ready && running && ChapterSeek::decode(bytes).is_some()
    }

    pub fn answer(&mut self, answer: UnloadAnswer) -> bool {
        if !matches!(*self, Self::AwaitingUnload(query) if query == answer.query) { return false; }
        *self = Self::Running { last_query: answer.query };
        true
    }

    /// Reject malformed or out-of-order reports without changing the session.
    pub fn receive<'a>(&mut self, bytes: &'a [u8]) -> Option<Report<'a>> {
        if bytes.len() > MAX_FRAME { return None; }
        let (report, rest) = postcard::take_from_bytes::<Report>(bytes).ok()?;
        if !rest.is_empty() || !report.valid() { return None; }
        *self = match (*self, report) {
            (Self::Waiting, Report::Started { .. }) => Self::Running { last_query: 0 },
            (running @ Self::Running { .. }, Report::Title(_) | Report::Resize { .. } | Report::MediaReady | Report::Navigate(_) | Report::UrlChanged(_) | Report::Submit { .. } | Report::PickFile | Report::Copy { .. } | Report::Paste { .. } | Report::OpenBeside(_)) => running,
            (Self::Running { last_query }, Report::BeforeUnload { query }) if query > last_query => Self::AwaitingUnload(query),
            (Self::Running { .. }, Report::Ended(_)) => Self::Ended,
            _ => return None,
        };
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_limits_apply_on_receipt_as_well_as_emission() {
        let long = "x".repeat(257);
        for report in [Report::Title(&long), Report::Title("a\nb"), Report::Resize { width: 0, height: 1 },
                       Report::Resize { width: 1, height: 16385 }, Report::Started { origin: "https://a/path" }] {
            assert!(report.encode().is_none());
            let mut state = if matches!(report, Report::Started { .. }) { Lifecycle::Waiting } else { Lifecycle::Running { last_query: 0 } };
            assert!(state.receive(&postcard::to_stdvec(&report).unwrap()).is_none());
        }
        assert!(Report::Title(&"x".repeat(256)).encode().is_some());
        assert!(Report::Resize { width: 16384, height: 1 }.encode().is_some());
    }

    #[test]
    fn generated_report_sequences_preserve_session_order() {
        use Report::*;
        let reports = [Started { origin: "https://widget.example" }, Title(""), Title("文"),
                       Resize { width: 1, height: 16384 }, Ended(EndReason::Closed), Ended(EndReason::Failed)];
        // Every five-report sequence, including every prefix and repeated terminal reports.
        for mut sequence in 0..reports.len().pow(5) {
            let mut state = Lifecycle::default();
            let (mut started, mut ended) = (false, false);
            for _ in 0..5 {
                let report = reports[sequence % reports.len()];
                sequence /= reports.len();
                let allowed = !ended && (matches!(report, Started { .. }) != started);
                let wire = report.encode().unwrap();
                assert_eq!(state.receive(&wire), allowed.then_some(report));
                if allowed {
                    started = true;
                    ended = matches!(report, Ended(_));
                }
            }
        }
    }

    #[test]
    fn a_navigation_report_is_a_plain_https_address_after_the_start() {
        let mut state = Lifecycle::default();
        let nav = Report::Navigate("https://app.example/second.html").encode().unwrap();
        assert!(state.receive(&nav).is_none(), "not before the page started");
        assert!(state.receive(&Report::Started { origin: "https://app.example" }.encode().unwrap()).is_some());
        assert_eq!(state.receive(&nav), Some(Report::Navigate("https://app.example/second.html")));
        assert!(state.receive(&Report::UrlChanged("https://app.example/route/1").encode().unwrap()).is_some());
        assert_eq!(state.receive(&Report::OpenBeside("https://elsewhere.example/").encode().unwrap()), Some(Report::OpenBeside("https://elsewhere.example/")));
        assert!(Report::OpenBeside("http://elsewhere.example/").encode().is_none());
        for bad in ["http://app.example/", "javascript:alert(1)", "https://a.example/x y", "https://a.example/\n"] {
            assert!(Report::Navigate(bad).encode().is_none(), "{bad}");
            assert!(state.receive(&postcard::to_stdvec(&Report::Navigate(bad)).unwrap()).is_none(), "{bad}");
        }
        let long = format!("https://a.example/{}", "x".repeat(URL_MAX));
        assert!(Report::UrlChanged(&long).encode().is_none());
        // A form: a plain HTTPS address and a bounded, printable body.
        let form = Report::Submit { url: "https://app.example/login", body: b"user=ana&next=%2F" };
        assert_eq!(state.receive(&form.encode().unwrap()), Some(form));
        let big = vec![b'a'; FORM_MAX];
        let at_max = Report::Submit { url: &format!("https://a.example/{}", "x".repeat(URL_MAX - 18)), body: &big };
        assert!(at_max.encode().is_some_and(|b| b.len() <= MAX_FRAME), "the longest form fits a frame");
        for bad in [Report::Submit { url: "http://app.example/login", body: b"a=1" },
                    Report::Submit { url: "https://app.example/login", body: b"a=1\n" },
                    Report::Submit { url: "https://app.example/login", body: &vec![b'a'; FORM_MAX + 1] }] {
            assert!(bad.encode().is_none());
            assert!(state.receive(&postcard::to_stdvec(&bad).unwrap()).is_none());
        }
    }

    #[test]
    fn a_page_size_is_bounded_and_taken_only_while_running() {
        let size = PageSize::new(1024, 620).unwrap();
        assert_eq!(PageSize::decode(&size.encode()), Some(size));
        for (w, h) in [(0, 1), (1, 0), (16385, 1), (1, 70000)] { assert!(PageSize::new(w, h).is_none(), "{w}x{h}"); }
        assert!(PageSize::decode(&[b'P', 0, 0, 1, 0]).is_none());
        assert!(PageSize::decode(&size.encode()[..4]).is_none());
        let mut state = Lifecycle::default();
        assert!(!state.accepts_control(&size.encode(), false), "not before the page started");
        let mut state = Lifecycle::Running { last_query: 0 };
        assert!(state.accepts_control(&size.encode(), false));
        assert!(!Lifecycle::Ended.accepts_control(&size.encode(), true));
    }

    #[test]
    fn a_picked_file_is_one_name_and_taken_only_while_running() {
        let chosen = FilePicked::Chosen { name: "notes.txt".into() };
        assert_eq!(FilePicked::decode(&chosen.encode().unwrap()), Some(chosen.clone()));
        assert_eq!(FilePicked::decode(&FilePicked::Cancelled.encode().unwrap()), Some(FilePicked::Cancelled));
        for bad in ["", "a/b", "a\nb", &"x".repeat(PICKED_NAME_MAX + 1)] {
            assert!(FilePicked::Chosen { name: bad.into() }.encode().is_none(), "{bad:?}");
        }
        assert!(FilePicked::decode(&[b'F', 2]).is_none());
        assert!(FilePicked::decode(&[b'F', 1, 0xff]).is_none(), "not UTF-8");
        assert!(FilePicked::decode(&[b'F', 0, 0]).is_none());
        let wire = chosen.encode().unwrap();
        assert!(!Lifecycle::default().accepts_control(&wire, false), "not before the page started");
        assert!(Lifecycle::Running { last_query: 0 }.accepts_control(&wire, false));
        // The request itself is a report the running page may make.
        let ask = Report::PickFile.encode().unwrap();
        assert!(Lifecycle::default().receive(&ask).is_none());
        assert_eq!(Lifecycle::Running { last_query: 0 }.receive(&ask), Some(Report::PickFile));
    }

    #[test]
    fn a_copy_and_a_paste_carry_a_press_and_bounded_text() {
        let t = [7; 16];
        let copy = Report::Copy { text: "hello", token: t }.encode().unwrap();
        assert_eq!(Lifecycle::Running { last_query: 0 }.receive(&copy), Some(Report::Copy { text: "hello", token: t }));
        assert!(Lifecycle::default().receive(&copy).is_none(), "not before the page runs");
        let big = "x".repeat(COPY_MAX);
        assert!(Lifecycle::Running { last_query: 0 }.receive(&Report::Copy { text: &big, token: t }.encode().unwrap()).is_some(), "at the bound");
        for bad in [Report::Copy { text: "", token: t }, Report::Copy { text: "a", token: [0; 16] }, Report::Paste { token: [0; 16] }] {
            assert!(bad.encode().is_none(), "{bad:?}");
            assert!(Lifecycle::Running { last_query: 0 }.receive(&postcard::to_stdvec(&bad).unwrap()).is_none());
        }
        assert!(Report::Copy { text: &"x".repeat(COPY_MAX + 1), token: t }.encode().is_none());
        let pasted = Pasted("ünïcode".into());
        assert_eq!(Pasted::decode(&pasted.encode().unwrap()), Some(pasted.clone()));
        assert!(Pasted(String::new()).encode().is_none() && Pasted("x".repeat(COPY_MAX + 1)).encode().is_none());
        assert!(Pasted::decode(&[b'V', 0xff]).is_none());
        assert!(Lifecycle::Running { last_query: 0 }.accepts_control(&pasted.encode().unwrap(), false));
        assert!(!Lifecycle::default().accepts_control(&pasted.encode().unwrap(), false));
    }

    #[test]
    fn a_refused_api_has_one_token_and_the_popovers_words() {
        let all = [Api::Notifications, Api::Geolocation, Api::Camera, Api::Microphone, Api::Midi, Api::Usb, Api::Bluetooth,
                   Api::Serial, Api::Hid, Api::ServiceWorker, Api::Push, Api::BackgroundSync, Api::ClipboardRead, Api::Popup, Api::LargeDownload];
        for api in all {
            assert_eq!(Api::from_token(api.token()), Some(api), "{api:?}");
            assert!(!api.label().is_empty());
        }
        for bad in ["", "Notifications", "notifications ", "camera\n", "x".repeat(4096).as_str()] {
            assert_eq!(Api::from_token(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn generated_field_boundaries_are_checked_by_the_decoder() {
        for n in 0..=MAX_FRAME + 1 {
            let text = "x".repeat(n);
            let report = Report::Title(&text);
            let bytes = postcard::to_stdvec(&report).unwrap();
            assert_eq!((Lifecycle::Running { last_query: 0 }).receive(&bytes), (n <= 256).then_some(report));
            assert!((Lifecycle::Running { last_query: 0 }).receive(&bytes[..bytes.len()-1]).is_none());
        }
    }

}
