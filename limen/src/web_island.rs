//! Reports from a broker-fetched web island (navigator-islands.md §5).
use serde::{Deserialize, Serialize};

/// The longest report: a submitted form's address, its body and framing.
pub const MAX_FRAME: usize = URL_MAX + FORM_MAX + 64;
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
            Self::Ended(_) | Self::MediaReady => true,
            Self::BeforeUnload { query } => *query != 0,
            Self::Navigate(url) | Self::UrlChanged(url) => reportable(url),
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
        if PageSize::decode(bytes).is_some() { return running; }
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
            (running @ Self::Running { .. }, Report::Title(_) | Report::Resize { .. } | Report::MediaReady | Report::Navigate(_) | Report::UrlChanged(_) | Report::Submit { .. }) => running,
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
