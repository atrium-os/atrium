//! ★ THE DISPLAY PATH IN THE WORKER (backend §4.8, bands step 1): from the
//! session's document to the scene of its first screen.
//!
//! Normalizing is about half of a big document's worker peak and seconds of
//! its time (§4.8, "where a big document's worker memory is"), and it is a
//! pure function of the recording. So it is done once per recording: the
//! first worker to show a recording's untouched initial state normalizes it
//! and hands the normalized document back with the scene; the broker stores
//! it by content address and gives it to every later worker showing that
//! same state, which then only lays out.
//!
//! A navigated document is a changed DOM, not the recording's: it is
//! normalized again from its current serialization, with the recording's
//! inputs (stylesheets, image sizes, sprites), and never cached.
//!
//! What a worker answers is the first screen only — `render_html_first` down
//! to the limit, then exactly the nodes that paint in `[0, limit)` — so an
//! answer is bounded by a screen, not by the document.

use navigator_render::{bands, fontset::FontSet, html};
use navigator_style::cascade::Env;

thread_local! {
    /// The pinned font set, loaded once per worker (loading canonicalizes
    /// every embedded face).
    static FONTS: std::cell::OnceCell<Result<FontSet, String>> = const { std::cell::OnceCell::new() };
}

fn with_fonts<T>(f: impl FnOnce(&FontSet) -> T) -> Result<T, String> {
    FONTS.with(|c| match c.get_or_init(|| FontSet::load().map_err(|e| format!("font set: {e}"))) {
        Ok(fonts) => Ok(f(fonts)),
        Err(e) => Err(e.clone()),
    })
}

/// What a worker answers for a scene request.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneAnswer {
    /// The first screen as NSG text.
    pub nsg: String,
    /// The normalized document, when this worker produced it for the
    /// recording's untouched initial state (for the broker to store).
    pub normalized: Option<Vec<u8>>,
}

/// The largest first-screen limit a request may ask for, px.
pub const MAX_LIMIT_PX: u32 = 8192;

/// The first screen of a document, down to `limit_px`.
///
/// - `recording`: the session's recording bytes (its normalizer inputs);
/// - `current`: the document as it is now, serialized — `None` when it is
///   the recording's untouched initial state;
/// - `cached`: a normalized document the broker holds for that initial
///   state (ignored for a navigated document).
pub fn first_screen(recording: &[u8], current: Option<&str>, cached: Option<&[u8]>, limit_px: u32) -> Result<SceneAnswer, String> {
    if limit_px == 0 || limit_px > MAX_LIMIT_PX { return Err(format!("limit {limit_px} px is outside 1..={MAX_LIMIT_PX}")) }
    let json = std::str::from_utf8(recording).map_err(|_| "the recording is not UTF-8")?;
    let env = Env::default();
    let limit = limit_px as i64 * 64;
    with_fonts(|fonts| {
        // ★ The recording is PARSED only when this worker normalizes: a
        // stored normalized document needs nothing from it but the image
        // sizes, which `subresources_from_recording_json` reads line by line
        // without building the recording (the HTML standard's is 16 MiB of
        // JSON; parsing it was most of the cached arm's peak, §4.8).
        let (normalized, produced) = match (current, cached) {
            (None, Some(c)) => (String::from_utf8(c.to_vec()).map_err(|_| "the cached normalized document is not UTF-8")?, false),
            (None, None) => {
                let r = navigator_normalize::recording::parse(json)?;
                (navigator_normalize::normalize_and_measure(&r.document, &r.inputs, fonts, &env).0, true)
            }
            (Some(doc), _) => {
                let r = navigator_normalize::recording::parse(json)?;
                (navigator_normalize::normalize_and_measure(doc, &r.inputs, fonts, &env).0, false)
            }
        };
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let (out, _) = html::render_html_first(&normalized, fonts, &env, &subs, limit);
        // Exactly what paints in the first screen, whatever the layout laid
        // out (it lays a page out whole when the first screen is unsafe).
        let idx = bands::index(&out.scene, limit);
        let mut first = bands::scene_of(&out.scene, &idx.nodes_in(0, limit));
        first.height = first.height.min(limit);
        let nsg = navigator_render::nsg::write(&first, fonts);
        Ok(SceneAnswer { nsg, normalized: produced.then(|| normalized.into_bytes()) })
    })?
}

// ---------------------------------------------------------------------------
// ★ BANDS OVER THE WIRE (pergola.md §10.4; backend §4.8): the worker holds a
// session's content source and answers `want` with the source's events and
// the groups it wrote, as NSG.
// ---------------------------------------------------------------------------

use navigator_render::source::{self, ContentSource, Event as SourceEvent, GroupId, Want};

/// What a view asks a session's content source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandsAsk {
    /// Keep `range` ready (1/64 px of the layout at `zoom_64`), with
    /// `prefetch` more either side.
    Want { range: (i64, i64), zoom_64: u32, prefetch: i64 },
    /// Background work: lay out up to `bands` more of the document.
    Idle { bands: u32 },
}

/// Bounds on an ask, checked by the broker before a worker sees it and by
/// the worker again.
pub const MAX_COORD: i64 = 1 << 40;
pub const MAX_IDLE_BANDS: u32 = 64;

impl BandsAsk {
    pub fn check(&self) -> Result<(), String> {
        match *self {
            BandsAsk::Want { range: (a, b), zoom_64, prefetch } => {
                if !(-MAX_COORD..=MAX_COORD).contains(&a) || !(-MAX_COORD..=MAX_COORD).contains(&b) || b < a { return Err(format!("range {a}..{b} is out of bounds")) }
                if !(crate::navigatord::MIN_ZOOM_64..=crate::navigatord::MAX_ZOOM_64).contains(&zoom_64) { return Err(format!("zoom {zoom_64}/64 is out of bounds")) }
                if !(0..=MAX_COORD).contains(&prefetch) { return Err(format!("prefetch {prefetch} is out of bounds")) }
                Ok(())
            }
            BandsAsk::Idle { bands } => if bands == 0 || bands > MAX_IDLE_BANDS { Err(format!("idle of {bands} bands is out of bounds")) } else { Ok(()) },
        }
    }
}

/// A group a source wrote: which, the band's range it is clipped to, NSG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPut { pub group: GroupId, pub clip: Option<(i64, i64)>, pub nsg: String }

/// What a worker answers for an ask.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BandsAnswer {
    pub events: Vec<SourceEvent>,
    /// Groups written, in order (a later put of a group replaces an earlier).
    pub puts: Vec<GroupPut>,
    pub clears: Vec<GroupId>,
    /// The normalized document, when this worker produced it for the
    /// recording's untouched initial state (for the broker to store).
    pub normalized: Option<Vec<u8>>,
}

/// A session's content source, in its worker: built on the first ask from
/// the normalized document (the stored one when the broker has it), and
/// built again when the document changes (`generation`).
pub struct Bands {
    src: source::DocumentSource<'static>,
    pub generation: u64,
}

/// ★ The pinned font set for a source that lives as long as its worker: a
/// worker holds one document, so this is loaded once and kept.
fn static_fonts() -> Result<&'static FontSet, String> {
    thread_local! { static F: std::cell::OnceCell<Result<&'static FontSet, String>> = const { std::cell::OnceCell::new() }; }
    F.with(|c| c.get_or_init(|| FontSet::load().map(|f| &*Box::leak(Box::new(f))).map_err(|e| format!("font set: {e}"))).clone())
}

struct NsgSink<'a> { fonts: &'a FontSet, puts: Vec<GroupPut>, clears: Vec<GroupId> }

impl source::SceneSink for NsgSink<'_> {
    fn put(&mut self, group: GroupId, clip: Option<(i64, i64)>, scene: &navigator_render::Scene) {
        self.clears.retain(|g| *g != group);
        self.puts.retain(|p| p.group != group);
        self.puts.push(GroupPut { group, clip, nsg: navigator_render::nsg::write(scene, self.fonts) });
    }
    fn clear(&mut self, group: GroupId) {
        self.puts.retain(|p| p.group != group);
        if !self.clears.contains(&group) { self.clears.push(group) }
    }
}

impl Bands {
    /// The source for a document: as `first_screen` chooses it (`recording`,
    /// `current`, `cached`).
    pub fn open(recording: &[u8], current: Option<&str>, cached: Option<&[u8]>, generation: u64) -> Result<(Bands, Option<Vec<u8>>), String> {
        let json = std::str::from_utf8(recording).map_err(|_| "the recording is not UTF-8")?;
        let env = Env::default();
        let fonts = static_fonts()?;
        let (normalized, produced) = match (current, cached) {
            (None, Some(c)) => (String::from_utf8(c.to_vec()).map_err(|_| "the cached normalized document is not UTF-8")?, false),
            (None, None) => {
                let r = navigator_normalize::recording::parse(json)?;
                (navigator_normalize::normalize_and_measure(&r.document, &r.inputs, fonts, &env).0, true)
            }
            (Some(doc), _) => {
                let r = navigator_normalize::recording::parse(json)?;
                (navigator_normalize::normalize_and_measure(doc, &r.inputs, fonts, &env).0, false)
            }
        };
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let back = produced.then(|| normalized.clone().into_bytes());
        let src = source::DocumentSource::new(normalized, fonts, subs, (env.width_px, env.height_px), (env.height_px * 64.0) as i64);
        Ok((Bands { src, generation }, back))
    }

    /// Answer one ask.
    pub fn ask(&mut self, a: BandsAsk) -> Result<BandsAnswer, String> {
        a.check()?;
        let mut sink = NsgSink { fonts: static_fonts()?, puts: vec![], clears: vec![] };
        let events = match a {
            BandsAsk::Want { range, zoom_64, prefetch } => self.src.want(Want { range, zoom_64, prefetch }, &mut sink),
            BandsAsk::Idle { bands } => self.src.idle(bands as usize, &mut sink),
        };
        Ok(BandsAnswer { events, puts: sink.puts, clears: sink.clears, normalized: None })
    }
}

/// ★ THE WIRE FORM of an answer (worker → broker), line based:
/// `<normalized_len>\n` and that many bytes; then one line per item —
/// `E extent <height> <0|1>`, `E ready|updated <band> <y0> <y1>`,
/// `E evicted <band>`, `C <group>`, `P <group> <y0|-> <y1|-> <len>` followed
/// by a newline and `len` bytes of NSG — where a group is `b<band>` or `o`.
pub fn encode_answer(a: &BandsAnswer) -> Vec<u8> {
    let g = |g: &GroupId| match g { GroupId::Band(b) => format!("b{b}"), GroupId::Overlay => "o".into() };
    let n = a.normalized.as_deref().unwrap_or(&[]);
    let mut out = format!("{}\n", n.len()).into_bytes();
    out.extend_from_slice(n);
    for e in &a.events {
        let line = match e {
            SourceEvent::Extent { height, settled } => format!("E extent {height} {}\n", *settled as u8),
            SourceEvent::BandReady { band, range } => format!("E ready {band} {} {}\n", range.0, range.1),
            SourceEvent::BandUpdated { band, range } => format!("E updated {band} {} {}\n", range.0, range.1),
            SourceEvent::BandEvicted { band } => format!("E evicted {band}\n"),
        };
        out.extend_from_slice(line.as_bytes());
    }
    for c in &a.clears { out.extend_from_slice(format!("C {}\n", g(c)).as_bytes()) }
    for p in &a.puts {
        let (y0, y1) = p.clip.map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or(("-".into(), "-".into()));
        out.extend_from_slice(format!("P {} {y0} {y1} {}\n", g(&p.group), p.nsg.len()).as_bytes());
        out.extend_from_slice(p.nsg.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Parse a worker's answer, strictly: every number is digits (with a sign
/// where a coordinate may be negative), every length within the payload,
/// and nothing else.
pub fn decode_answer(p: &[u8]) -> Result<BandsAnswer, String> {
    let mut at = 0usize;
    let line = |at: &mut usize| -> Result<&str, String> {
        let rest = &p[*at..];
        let nl = rest.iter().take(256).position(|&b| b == b'\n').ok_or("a line is missing or too long")?;
        let s = std::str::from_utf8(&rest[..nl]).map_err(|_| "a line is not text")?;
        *at += nl + 1;
        Ok(s)
    };
    let num = |v: &str| -> Result<i64, String> {
        let d = v.strip_prefix('-').unwrap_or(v);
        if d.is_empty() || d.len() > 19 || !d.bytes().all(|b| b.is_ascii_digit()) { return Err(format!("{v:?} is not a number")) }
        v.parse().map_err(|_| format!("{v:?} is not a number"))
    };
    let unum = |v: &str| -> Result<u64, String> {
        if v.is_empty() || v.len() > 19 || !v.bytes().all(|b| b.is_ascii_digit()) { return Err(format!("{v:?} is not a count")) }
        v.parse().map_err(|_| format!("{v:?} is not a count"))
    };
    let group = |v: &str| -> Result<GroupId, String> {
        if v == "o" { return Ok(GroupId::Overlay) }
        let b = v.strip_prefix('b').ok_or_else(|| format!("{v:?} is not a group"))?;
        Ok(GroupId::Band(u32::try_from(unum(b)?).map_err(|_| "band id out of range")?))
    };
    let mut a = BandsAnswer::default();
    let n = unum(line(&mut at)?)? as usize;
    if n > crate::navigatord::MAX_NORMALIZED_BYTES || at + n > p.len() { return Err("normalized length out of bounds".into()) }
    if n > 0 { a.normalized = Some(p[at..at + n].to_vec()) }
    at += n;
    while at < p.len() {
        let l = line(&mut at)?.to_string();
        let f: Vec<&str> = l.split(' ').collect();
        match f.as_slice() {
            ["E", "extent", h, s] => a.events.push(SourceEvent::Extent { height: num(h)?, settled: match *s { "0" => false, "1" => true, _ => return Err("settled is not 0 or 1".into()) } }),
            ["E", k @ ("ready" | "updated"), b, y0, y1] => {
                let band = u32::try_from(unum(b)?).map_err(|_| "band id out of range")?;
                let range = (num(y0)?, num(y1)?);
                a.events.push(if *k == "ready" { SourceEvent::BandReady { band, range } } else { SourceEvent::BandUpdated { band, range } });
            }
            ["E", "evicted", b] => a.events.push(SourceEvent::BandEvicted { band: u32::try_from(unum(b)?).map_err(|_| "band id out of range")? }),
            ["C", g] => a.clears.push(group(g)?),
            ["P", g, y0, y1, len] => {
                let clip = match (*y0, *y1) { ("-", "-") => None, (a0, b0) => Some((num(a0)?, num(b0)?)) };
                let len = unum(len)? as usize;
                if at + len + 1 > p.len() || p[at + len] != b'\n' { return Err("a group's length is out of bounds".into()) }
                let nsg = String::from_utf8(p[at..at + len].to_vec()).map_err(|_| "a group is not UTF-8")?;
                at += len + 1;
                a.puts.push(GroupPut { group: group(g)?, clip, nsg });
            }
            _ => return Err(format!("unknown line {:?}", l.chars().take(40).collect::<String>())),
        }
    }
    Ok(a)
}
