//! The window's state and what each press does (docs/spec/navigator-ui.md
//! §5): the trail store, the rail's width, the Where field while it is
//! being typed in, the status bar switch, the tier popover. Drawing is in
//! [`crate::ui`]; this module is plain state so it can be tested without
//! a frescod.

use pergola::text_edit::{KeyOutcome, TextEdit};
use pergola::{Key, Modifiers, Rect};

use crate::fake::{self, Link};
use crate::keys::Action;
use crate::trail::{Asked, Facts, Light, Media, PageId, PageSpec, Prompt, TrailStore, PROMPTS_QUEUED, PROMPT_EVERY_MS, PROMPT_STILL_MS};
use crate::where_field::{self, Entry, Source, Suggestion};

/// Rail width, wide and collapsed (§5.2; collapsed = Forum's dock rail).
pub const RAIL: f32 = 248.0;
pub const RAIL_NARROW: f32 = 56.0;
/// Below this window width the rail collapses by itself.
pub const RAIL_AUTO_NARROW: f32 = 900.0;
pub const BAR: f32 = 40.0;

/// `pt` at the system text size (resolution-independence.md §6.2): the
/// toolbar and what hangs from it grow with their text.
pub fn em(pt: f32) -> f32 { pergola::theme::size::em(pt) }

/// The toolbar's height: [`BAR`] at the text size.
pub fn bar() -> f32 { em(BAR) }

/// [`bar`] at text size `k` (for tests and layout checks).
pub fn bar_at(k: f32) -> f32 { BAR * k }
pub const PROMPT: f32 = 44.0;
pub const STATUS: f32 = 24.0;
/// The suggestion list's rows and section labels.
pub const DROP_ROW: f32 = 30.0;
pub const DROP_LABEL: f32 = 22.0;
pub const POPOVER_W: f32 = 380.0;
/// The find bar's height (§7), under the toolbar.
pub const FIND: f32 = 40.0;
/// The Aa pop-up (§7).
pub const READING_W: f32 = 300.0;
pub const READING_H: f32 = 236.0;
/// The history view's rows and day labels (§5.5).
pub const HIST_ROW: f32 = 34.0;
pub const HIST_DAY: f32 = 28.0;
/// A day label's Clear day button, and the clear confirmation's height.
pub const HIST_DAY_CLEAR_W: f32 = 72.0;
pub const HIST_CONFIRM: f32 = 56.0;

/// The history view's Clear all button: its x and width, for a column at
/// `x` that is `col` wide (left of the close button).
pub fn history_clear_all(x: f32, col: f32) -> (f32, f32) {
    (x + col - em(36.0) - em(84.0), em(84.0))
}

/// The page slot's key (pergola::slot): where Limen binds the page.
pub const PAGE_SLOT: u64 = 0x5041_4745; // "PAGE"
/// The print sheet's slot, over the whole window while it is open.
pub const PRINT_SLOT: u64 = 0x5052_4e54; // "PRNT"
/// Arca's password field in a VPN sign-in window's form panel.
pub const ARCA_SLOT: u64 = 0x4152_4341; // "ARCA"

/// The Where field while it is focused (§5.3).
#[derive(Clone, Debug)]
pub struct WhereEdit {
    pub edit: TextEdit,
    /// The highlighted suggestion (arrow keys, hover), if any.
    pub selected: Option<usize>,
}

/// The find bar (§7): what is typed, and what the page child said of it.
#[derive(Clone, Debug)]
pub struct FindBar {
    pub edit: TextEdit,
    /// The page it finds in: another page shown closes it.
    pub key: Option<(u64, PageId)>,
    pub count: u32,
    pub current: u32,
    /// Whether the page has answered since the last change.
    pub answered: bool,
}

/// The history view (§5.5): its filter and the highlighted row.
#[derive(Clone, Debug)]
pub struct HistoryView {
    pub edit: TextEdit,
    pub selected: usize,
    /// A clear asked for and waiting for its confirmation, with how many
    /// pages it takes (it can't be undone).
    pub confirm: Option<(crate::history::Clear, usize)>,
}

#[derive(Clone, Debug)]
pub struct Shell {
    pub store: TrailStore,
    /// This window's named persona (personas.md §3: one per window), from
    /// navigatord's settings (`NAVIGATOR_UI_PERSONA`); `None` is Personal.
    pub window_persona: Option<String>,
    /// The person's other personas (`personal` for Personal), from
    /// navigatord (`NAVIGATOR_UI_OTHER_PERSONAS`): the New window menu.
    pub other_personas: Vec<String>,
    /// The rail's persona chip menu is open: new trails' persona, and a
    /// New window in another persona.
    pub persona_open: bool,
    pub w: f32,
    pub h: f32,
    /// Ctrl-B, or the rail's sidebar button.
    pub rail_collapsed: bool,
    /// The optional status bar (§5.7, D11: off by default; Ctrl-/).
    pub status_bar: bool,
    pub editing: Option<WhereEdit>,
    /// The tier chip's popover (§6.3, §6.5).
    pub popover: bool,
    /// Modifiers held, from the last key event (Ctrl-click opens beside).
    pub mods: Modifiers,
    /// A link to show in the status bar when the pointer is on none (the
    /// screenshot scenes; the real hover comes from the page in step 2).
    pub hover_hint: Option<String>,
    /// The last thing a press did that has no visible effect yet in step 1
    /// (a prompt's answer, Reload), for the log.
    pub last_note: Option<String>,
    /// The print sheet is open (or the page is being laid out for it).
    pub printing: bool,
    /// Pages come from navigatord (step 2): `None` is the stand-in store.
    pub live: Option<LivePage>,
    /// What the UI asks navigatord next (drained by the main loop).
    pub outbox: Vec<Out>,
    /// Where the prompt row was last drawn: a row that moves starts its
    /// 500 ms again (§6.4).
    pub prompt_at: Option<Rect>,
    /// Ctrl-F (§7).
    pub find: Option<FindBar>,
    /// The shown page's sections (anchor, heading), from its `Loaded`
    /// (§7 "Jump to section"), and whether their list is open.
    pub sections: Vec<(String, String)>,
    pub sections_open: bool,
    /// The last answer sent to navigatord with a press, and the page it
    /// was on: put back if frescod didn't count the press.
    pub answered: Option<((u64, PageId), Prompt)>,
    /// The Aa pop-up is open (§7).
    pub reading_open: bool,
    /// Ctrl-H (§5.5): drawn where the page is, the page's slot withdrawn.
    pub history: Option<HistoryView>,
    /// The presence menu is open (§6.2), and what Arca said about the
    /// accounts it holds for the site it was opened on.
    pub presence_open: bool,
    pub accounts: Option<(String, Accounts)>,
    /// F6 put the keys on the rail (§9): Up and Down go through the
    /// trails, Enter or Escape leaves it.
    pub rail_focus: bool,
    /// F6 asked for the page to have the keys: the main loop hands them
    /// over (frescod `OP_WINDOW_FOCUS_EMBED`) and clears it.
    pub focus_page: bool,
    /// The network's name when this is its sign-in window (atrium-net.md
    /// §8.1 S3; `NAVIGATOR_UI_PORTAL` from navigatord): the only window
    /// whose form panel shows and sends a page's forms.
    pub portal: Option<String>,
    /// The sign-in window's form panel, for the page shown.
    pub form: Option<FormPanel>,
    /// Whole pages may run live on a press (`NAVIGATOR_UI_LIVE_PAGES=1`,
    /// set by navigatord while I6.1 is built).
    pub live_pages: bool,
    /// The page running live, or asked to: its address, and its origin
    /// once the engine started (navigator-islands.md I6.1).
    pub live_run: Option<LiveRun>,
    /// The origins navigatord said the persona runs live every time (D8,
    /// `Event::LiveRemembered`), at most [`LIVE_REMEMBERED_MAX`]: the chip
    /// offers to ask again for them. navigatord's list is the one that
    /// counts; this only says which row to show.
    pub live_remembered: Vec<String>,
    /// The files answered for this run (§15.1 Downloads), newest last, at
    /// most [`DOWNLOADS_KEPT`]; whether one changed since the list was
    /// last open (the button's dot); and whether the list is open.
    pub downloads: Vec<Download>,
    pub downloads_new: bool,
    pub downloads_open: bool,
    /// The More menu (§15.1).
    pub more_open: bool,
    /// The share sheet is up for the page (share-sheet.md §5), one at a time.
    pub sharing: bool,
    /// Find in trails (§15.1): what is typed, kept while the rail shows
    /// only the trails it matches; and whether the field has the keys.
    pub rail_find: Option<TextEdit>,
    pub rail_find_focus: bool,
}

/// `s` without control characters and without the invisible marks that
/// reorder or hide text (bidi embeddings, overrides and isolates, the
/// direction marks, zero-width joiners): a name a site chose, as the
/// chrome shows it or hands it on, reads as its characters do.
pub fn plain(s: &str) -> String {
    s.chars().filter(|&c| !c.is_control() && !matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')).collect()
}

/// Start's column (§15.1): its width, and where its field sits below the
/// toolbar, and how tall it is.
pub const START_COL: f32 = 640.0;
pub const START_FIELD_Y: f32 = 96.0;
pub const START_FIELD_H: f32 = 44.0;

/// The rail's foot (All history) and its pinned tiles (§15.1).
pub const RAIL_FOOT: f32 = 40.0;
/// The longest Find in trails query kept.
pub const RAIL_QUERY_MAX: usize = 256;
pub const TILE: f32 = 36.0;
pub const TILE_GAP: f32 = 8.0;

/// The most downloads the list keeps.
pub const DOWNLOADS_KEPT: usize = 16;

/// A file the person answered Save or Open for (§15.1 Downloads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub id: u32,
    pub file: String,
    pub size: String,
    pub origin: String,
    pub state: DownloadState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Saving,
    Opening,
    /// Where navigatord saved it.
    Saved(String),
    /// The app it opened in.
    Opened(String),
    /// Why it was not saved or opened.
    Failed(String),
}

/// What the presence pill says (§15.1): whether the site knows you, and
/// as whom when Arca holds an account for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    NotKnown,
    Remembers,
    SignedIn(String),
}

/// A row of the More menu (§15.1). Reading and Share are there only when
/// the window is too narrow for their buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoreRow {
    Reading,
    Share,
    OpenWith,
    Pin,
    Unpin,
    Find,
    Print,
    History,
    StatusBar,
}

impl MoreRow {
    pub fn label(self) -> &'static str {
        match self {
            MoreRow::Reading => "Reading controls",
            MoreRow::Share => "Share\u{2026}",
            MoreRow::OpenWith => "Open with\u{2026}",
            MoreRow::Pin => "Pin this trail",
            MoreRow::Unpin => "Unpin this trail",
            MoreRow::Find => "Find in page",
            MoreRow::Print => "Print\u{2026}",
            MoreRow::History => "History",
            MoreRow::StatusBar => "Status bar",
        }
    }

    pub fn keys(self) -> &'static str {
        match self {
            MoreRow::Find => "Ctrl F",
            MoreRow::Print => "Ctrl P",
            MoreRow::History => "Ctrl H",
            MoreRow::StatusBar => "Ctrl /",
            _ => "",
        }
    }
}

/// The toolbar (§15.1): the icon buttons at its right, each a 28 pt square
/// on a 32 pt pitch, and the presence pill before them.
pub const TOOL_PITCH: f32 = 32.0;
pub const PRESENCE_PILL: f32 = 148.0;
pub const PRESENCE_ICON: f32 = 32.0;
/// Below these widths of the area right of the rail, the presence pill
/// keeps only its icon, then Reading and Share move into More (§15.1).
pub const BAR_PRESENCE_LABEL: f32 = 760.0;
pub const BAR_TOOLS_FOLD: f32 = 620.0;
/// The More menu and the Downloads list.
pub const MORE_W: f32 = 260.0;
pub const MORE_ROW: f32 = 32.0;
pub const DOWNLOADS_W: f32 = 340.0;
pub const DOWNLOAD_ROW: f32 = 48.0;
/// The share sheet's slot and the chosen app's (share-sheet.md §5).
pub const SHARE_SLOT: u64 = 0x5348_5245; // "SHRE"
pub const SHARE_TARGET_SLOT: u64 = 0x5348_5447; // "SHTG"

/// The most remembered origins the UI keeps track of.
pub const LIVE_REMEMBERED_MAX: usize = 256;

/// The form panel (navigator-ui.md "Forms in the sign-in window"): the
/// shown page's first plain form as its page child read it, with what the
/// person types and ticks here, in the chrome; the page never sees it.
#[derive(Clone, Debug)]
pub struct FormPanel {
    pub key: Option<(u64, PageId)>,
    pub form: limen::doc_viewer::Form,
    /// One per field; only text and password fields' are used.
    pub edits: Vec<TextEdit>,
    pub checked: Vec<bool>,
    /// The text field with the keys.
    pub focus: Option<usize>,
    /// Arca's handle for what was typed into its field (a VPN sign-in
    /// window's password field, [`FormPanel::arca_field`]).
    pub arca: Option<[u8; 16]>,
    /// A press on a button before Arca's handle came (the field holds what
    /// was typed when it loses the keys, which the press itself does): the
    /// form goes when the handle comes, or without it at the deadline.
    pub waiting: Option<(usize, Option<navigator_control::Press>, std::time::Instant)>,
    /// A VPN sign-in window's panel: its password is Arca's field.
    pub vpn: bool,
    /// Counts the forms sent: Arca's field is asked for anew after each
    /// (its handle went with the form).
    pub sent: u32,
}

impl FormPanel {
    /// The fields the panel shows a row for (text, password, checkbox,
    /// radio), by index.
    pub fn rows(&self) -> Vec<usize> {
        use limen::doc_viewer::FieldKind::*;
        self.form.fields.iter().enumerate().filter(|(_, f)| matches!(f.kind, Text | Password | Checkbox | Radio)).map(|(i, _)| i).collect()
    }

    /// The buttons that send it, by index.
    pub fn buttons(&self) -> Vec<usize> {
        self.form.fields.iter().enumerate().filter(|(_, f)| f.kind == limen::doc_viewer::FieldKind::Submit).map(|(i, _)| i).take(4).collect()
    }

    pub fn texts(&self) -> Vec<usize> {
        use limen::doc_viewer::FieldKind::*;
        let arca = self.arca_field();
        self.rows().into_iter().filter(|&i| matches!(self.form.fields[i].kind, Text | Password) && Some(i) != arca).collect()
    }

    /// The password field Arca draws (in its own surface, in
    /// [`ARCA_SLOT`]): a VPN sign-in window's first, so the password never
    /// reaches this process or navigatord (arca-autofill.md §3).
    pub fn arca_field(&self) -> Option<usize> {
        if !self.vpn {
            return None;
        }
        self.rows().into_iter().find(|&i| self.form.fields[i].kind == limen::doc_viewer::FieldKind::Password)
    }
}

/// How long a press waits for Arca's handle.
pub const ARCA_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// The form panel's rows and its heading.
pub const FORM_ROW: f32 = 36.0;
pub const FORM_HEAD: f32 = 36.0;
/// A form button's width.
pub const FORM_BUTTON_W: f32 = 160.0;
/// The most rows the panel shows; a longer form shows its first ones.
pub const FORM_ROWS_SHOWN: usize = 8;

/// What F6 moves the keys between (§9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Rail,
    Toolbar,
    Page,
}

/// `https://host[:port]` of a plain HTTPS address.
fn origin_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    (end > 0).then(|| &url[..8 + end])
}

/// A whole page running live (I6.1).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveRun {
    pub key: (u64, PageId),
    pub url: String,
    /// `None` until navigatord reports the engine started.
    pub origin: Option<String>,
    /// The trail entries this run made or went to (I6.2), `key` among
    /// them: moving to one runs it live again with no new press.
    pub entries: Vec<(u64, PageId)>,
    /// What the page was refused (I6.4), in the popover's words, each
    /// once, at most [`LIVE_REFUSED_MAX`].
    pub refused: Vec<String>,
}

/// The most refusals the popover names for one run.
pub const LIVE_REFUSED_MAX: usize = 12;

/// The live page's state, as navigatord and the page child reported it.
#[derive(Clone, Debug, PartialEq)]
pub enum LivePage {
    /// Asked for, not yet converted.
    Loading(String),
    LiveOffer,
    /// Opened and handed to the page child.
    Shown(String),
    /// navigatord could not open it.
    Failed { url: String, why: String },
    /// Not a page but a file (§6.4): the prompt row asks what to do.
    File { url: String, name: String },
}

/// A request for navigatord the shell can't send itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A link pressed in the page (the href as the page spells it).
    /// `jar`: the trail's, where a consent control's press keeps its
    /// cookies (§6.2).
    Activate { page: String, href: String, beside: bool, jar: navigator_control::Jar },
    /// The person told a site to forget them in a jar (§6.2).
    Forget { jar: navigator_control::Jar, url: String },
    /// Save, or drop, the file navigatord holds under that `Open` (§6.4).
    /// `press`: frescod's token for the press on the answer, which
    /// navigatord spends (the row seen uncovered by other windows).
    Save { id: u32, press: navigator_control::Press },
    Discard { id: u32 },
    /// Open that file in the person's app for it (§6.4), with the press.
    OpenFile { id: u32, press: navigator_control::Press },
    AnswerUnload { start: navigator_control::Press, query: u32, leave: bool, press: navigator_control::Press },
    StartWidget { page: (u64, PageId), key: u32, src: String, press: navigator_control::Press },
    RunLive { url: String, jar: navigator_control::Jar, press: navigator_control::Press },
    /// Run the page on screen live (I6.1), with the press on "Run live".
    RunPageLive { url: String, press: navigator_control::Press, jar: navigator_control::Jar, remember: bool },
    /// The chip's "Ask before running live" (D8).
    ForgetLive { url: String },
    /// Stop the page running live: back to the document lane.
    StopLive { url: String },
    /// Run `url` live too (I6.2): the live page went there, same origin.
    ContinueLive { url: String, jar: navigator_control::Jar },
    /// Hand a link to its program (§6.4, threat model §2.10).
    Hand { url: String, how: navigator_control::Hand, press: navigator_control::Press },
    /// A Navigator window in another persona (§2.4), with the press.
    NewWindow { persona: String, press: navigator_control::Press },
    /// Send the sign-in page's form (atrium-net.md §8.1 P5), with the press
    /// on its button.
    SubmitForm { page: String, jar: navigator_control::Jar, action: String, post: bool, fields: Vec<navigator_control::FormField>, press: navigator_control::Press },
    /// Find in the page on screen (§7; limen `ToChild::Find`).
    Find { query: String, step: i8 },
    /// The current version of the page `key`, fetched again (§5.5).
    Reload { key: (u64, PageId) },
    /// "Sign out here" (§6.2): the session cookies of the page's site in
    /// its jar end (then the page opens again, `Reload`).
    SignOut { jar: navigator_control::Jar, url: String },
    /// "Jump to section" (§7): the page shown scrolls to `anchor`.
    Jump { anchor: String },
    /// Print the page on screen (§7 "Print"): its child lays it out on
    /// paper and the print sheet opens over the window.
    Print,
    /// Share the page's address through the system's share sheet, with
    /// the press on Share, which the sheet spends (share-sheet.md §5).
    Share { url: String, title: String, press: navigator_control::Press },
    /// A branch made its own trail (§4.2): the moved pages' keys, old to
    /// new, so their live children follow them.
    Moved { keys: Vec<((u64, PageId), (u64, PageId))> },
    /// Which accounts Arca holds for the page's site (the presence menu).
    Accounts { url: String },
    /// History cleared (§5.5): the pages that went (their children close)
    /// and the stored versions navigatord takes out.
    Clear { keys: Vec<(u64, PageId)>, records: Vec<String> },
}

/// The accounts Arca holds for a site, as the presence menu shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accounts {
    /// navigatord was asked.
    Asking,
    Known(Vec<navigator_control::Account>),
    /// Arca couldn't be asked, and why.
    Unknown(String),
}

/// The presence menu's size (§6.2): a heading, the account lines, and
/// its one or two actions.
pub const PRESENCE_W: f32 = 300.0;
/// The "Jump to section" list: its width, a row, the most rows shown.
pub const SECTIONS_W: f32 = 320.0;
pub const SECTION_ROW: f32 = 30.0;
pub const SECTIONS_SHOWN: usize = 12;
pub fn presence_height(s: &Shell) -> f32 {
    let knows = s.store.trail().page().is_some_and(|p| s.store.knows(crate::trail::site(&p.url)));
    if !knows {
        return em(100.0);
    }
    let lines = match &s.accounts {
        Some((_, Accounts::Known(v))) if !v.is_empty() => v.len().min(4),
        _ => 1,
    };
    // Heading 28, label 16, the lines, 8, two rows, the margins.
    em(14.0 + 28.0 + 16.0 + 20.0 * lines as f32 + 8.0 + 34.0 + 32.0 + 10.0)
}

/// A page about to be opened by navigatord: its title is its host until
/// navigatord names it, and its light is what its scheme earns (§6.5).
pub fn live_spec(url: &str) -> PageSpec {
    let insecure = url.starts_with("http:");
    let host = crate::trail::host(url).to_string();
    let conn = if insecure { "Not private: http, anyone on the path can read and change it" } else { "Private: https" };
    let facts = Facts { insecure, ..Facts::green(conn) }.settle();
    PageSpec { url: url.into(), title: host, search: None, facts, media: Media::None }
}

/// Where everything is, from the window's size and state.
#[derive(Clone, Debug)]
pub struct Geo {
    pub w: f32,
    pub h: f32,
    /// The rail's width; the content starts one seam to its right.
    pub rail: f32,
    pub x0: f32,
    pub where_: Rect,
    pub prompt: Option<Rect>,
    pub page: Rect,
    pub status: Option<Rect>,
    /// The suggestion list, while the Where field is typed in.
    pub drop: Option<Rect>,
    pub popover: Option<Rect>,
    /// The find bar, under the toolbar.
    pub find: Option<Rect>,
    /// The Aa pop-up, and the Aa button it hangs from.
    pub reading: Option<Rect>,
    pub aa: Rect,
    /// The presence control, and its menu while open (§6.2).
    pub presence: Rect,
    pub presence_menu: Option<Rect>,
    /// The rail's persona chip and its menu.
    pub persona_chip: Rect,
    pub persona_menu: Option<Rect>,
    /// The "Jump to section" list under the find bar, and its button.
    pub sections_menu: Option<Rect>,
    pub sections_button: Option<Rect>,
    /// Space on the toolbar for the current page's media glyph.
    pub media: bool,
    /// The sign-in window's bar: the network's name and the page's form.
    pub form: Option<Rect>,
    /// Keep, inside the Where field's right end (§15.1).
    pub keep: Rect,
    /// The presence pill shows its words (else only its icon).
    pub presence_label: bool,
    /// Downloads, Share (none when folded into More), More, and the
    /// Downloads list and More menu while open.
    pub downloads: Rect,
    pub share: Option<Rect>,
    pub more: Rect,
    pub downloads_menu: Option<Rect>,
    pub more_menu: Option<Rect>,
    /// The rail's Find in trails field and its All history row (§15.1),
    /// when the rail is wide.
    pub rail_find: Option<Rect>,
    pub all_history: Option<Rect>,
}

/// The network's name when this window is its sign-in window
/// (`NAVIGATOR_UI_PORTAL`), cleaned and cut.
pub fn window_portal() -> Option<String> {
    let n: String = std::env::var("NAVIGATOR_UI_PORTAL").ok()?.chars().filter(|c| !c.is_control()).collect();
    let n = limen::doc_viewer::cut(n.trim(), 64);
    (!n.is_empty()).then_some(n)
}

/// Whether this sign-in window is a corporate VPN's (`NAVIGATOR_UI_VPN`,
/// vpn.md §9.3) rather than a network's captive portal.
pub fn window_vpn() -> bool {
    std::env::var("NAVIGATOR_UI_VPN").is_ok_and(|v| v == "1")
}

/// The window's named persona from the settings, capitalised for the rail
/// ("work" shows as "Work"), if it is a fit name.
pub fn window_persona() -> Option<String> {
    fit_persona(&std::env::var("NAVIGATOR_UI_PERSONA").ok()?)
}

/// The person's other personas from the settings, each a fit name.
pub fn other_personas() -> Vec<String> {
    std::env::var("NAVIGATOR_UI_OTHER_PERSONAS").unwrap_or_default().split(',')
        .filter(|p| fit_persona(p).is_some()).take(8).map(String::from).collect()
}

/// The persona chip menu's width and row height.
pub const PERSONA_W: f32 = 236.0;
pub const PERSONA_ROW: f32 = 32.0;

/// A persona's name as shown: "work" as "Work", if it is a fit name.
pub fn fit_persona(p: &str) -> Option<String> {
    if p.is_empty() || p.len() > 32 || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    let mut c = p.chars();
    c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
}

impl Shell {
    pub fn new(store: TrailStore, w: f32, h: f32) -> Self {
        Shell { store, window_persona: window_persona(), other_personas: other_personas(), persona_open: false, w, h, rail_collapsed: false, status_bar: false, editing: None, popover: false, mods: Modifiers::default(), hover_hint: None, last_note: None, printing: false, live: None, outbox: Vec::new(), prompt_at: None, find: None, sections: Vec::new(), sections_open: false, answered: None, reading_open: false, history: None, presence_open: false, accounts: None, rail_focus: false, focus_page: false, portal: window_portal(), form: None, live_pages: std::env::var("NAVIGATOR_UI_LIVE_PAGES").as_deref() == Ok("1"), live_run: None, live_remembered: Vec::new(), downloads: Vec::new(), downloads_new: false, downloads_open: false, more_open: false, sharing: false, rail_find: None, rail_find_focus: false }
    }

    /// A page for `url`: from navigatord when live, else the stand-in.
    fn spec(&self, url: &str) -> PageSpec {
        if self.live.is_some() { live_spec(url) } else { fake::spec(url) }
    }

    /// The address the current trail shows, if it shows a page.
    pub fn shown_url(&self) -> Option<String> {
        self.store.trail().page().map(|p| p.url.clone())
    }

    /// The page on screen as (its trail's id, its id in the trail).
    pub fn shown_key(&self) -> Option<(u64, PageId)> {
        let t = self.store.trail();
        t.current.map(|c| (t.id, c))
    }

    /// navigatord opened `url` (after any redirect) for the
    /// page `key`, as the version at `saved`.
    pub fn opened(&mut self, key: (u64, PageId), url: &str, title: &str, insecure: bool, saved: &str, replayed: bool) {
        if let Some(p) = self.store.page_by_key_mut(key.0, key.1) {
            p.url = url.to_string();
            if !title.is_empty() {
                p.title = title.to_string();
            }
            let mut facts = live_spec(url).facts;
            if replayed {
                facts.tier = crate::trail::Tier::Replayed;
            }
            facts.insecure |= insecure;
            p.facts = facts.settle();
            p.saved = navigator_control::is_address(saved).then(|| saved.to_string());
        }
    }

    /// A link pressed in the live page: navigatord decides what it does.
    pub fn link_pressed(&mut self, href: &str) {
        if let Some(page) = self.shown_url() {
            self.outbox.push(Out::Activate { page, href: href.into(), beside: self.mods.ctrl, jar: self.store.trail().jar() });
        }
    }

    /// navigatord's decision: follow (or open beside) a link.
    pub fn follow_link(&mut self, url: &str, beside: bool) {
        let spec = self.spec(url);
        if beside {
            self.store.trail_mut().open_beside(spec);
        } else {
            self.store.follow(spec);
        }
    }

    /// The stand-in scenes of navigator-ui.md §5.8 (and a stop page).
    pub fn scene(sc: fake::Scene, w: f32, h: f32) -> Self {
        let (store, prompt) = fake::scene(sc);
        let mut s = Shell::new(store, w, h);
        if let Some(p) = prompt {
            s.ask(p, -PROMPT_STILL_MS);
        }
        match sc {
            fake::Scene::Main => {
                // Two pinned trails for the tiles (§15.1): Mail and the Handbook.
                for site in ["mail.example", "freebsd.example"] {
                    if let Some(i) = s.store.trails.iter().position(|t| t.page().is_some_and(|p| crate::trail::site(&p.url) == site)) {
                        s.store.toggle_pin(i);
                    }
                }
                s.status_bar = true;
                s.hover_hint = Some("https://atlas.example/wiki/Impluvium".into());
            }
            fake::Scene::Start => {
                s.rail_collapsed = true;
                s.store.new_trail(false);
                s.begin_edit();
                if let Some(e) = &mut s.editing {
                    e.edit.set_text("impl");
                }
            }
            fake::Scene::Live => {
                s.rail_collapsed = true;
                s.status_bar = true;
                s.hover_hint = Some("https://n0rthbank.example/login".into());
            }
            fake::Scene::Reading => {
                s.reading_change(|r| {
                    r.text_64 = 80;
                    r.comfortable = true;
                });
                s.reading_open = true;
                s.open_find();
                if let Some(f) = &mut s.find {
                    f.edit.set_text("atrium");
                    (f.count, f.current) = (4, 2);
                }
            }
            fake::Scene::History => {
                let now = crate::clock::now_s();
                // Spread the stand-in pages over a few days.
                for (i, t) in s.store.trails.iter_mut().enumerate() {
                    for (j, p) in t.pages.iter_mut().enumerate() {
                        p.at_s = now.saturating_sub(i as u64 * 86_400 + j as u64 * 1_400);
                    }
                }
                for (i, e) in s.store.earlier.iter_mut().enumerate() {
                    for (j, p) in e.trail.pages.iter_mut().enumerate() {
                        p.at_s = e.at_s.min(now).saturating_sub(i as u64 * 3_600 + j as u64 * 600);
                    }
                }
                s.open_history();
                if let Some(h) = &mut s.history {
                    h.selected = 1;
                }
            }
            fake::Scene::Prompt | fake::Scene::Stop => {}
        }
        s
    }

    pub fn rail_w(&self) -> f32 {
        if self.rail_collapsed || self.w < em(RAIL_AUTO_NARROW) { em(RAIL_NARROW) } else { em(RAIL) }
    }

    pub fn suggestions(&self) -> Vec<Suggestion> {
        match &self.editing {
            Some(e) => where_field::suggest(&self.store, e.edit.text()),
            None => vec![],
        }
    }

    pub fn geo(&self) -> Geo {
        let (w, h) = (self.w, self.h);
        let rail = self.rail_w();
        let x0 = rail + 1.0;
        let media = self.store.trail().page().is_some_and(|p| p.media != crate::trail::Media::None);
        let fx = x0 + em(111.0);
        // Right to left: More, Share, Downloads, Aa (Share and Aa fold
        // into More when narrow), then the presence pill, then media.
        let room = w - x0;
        let fold = room < em(BAR_TOOLS_FOLD);
        let presence_label = room >= em(BAR_PRESENCE_LABEL);
        let tool = |i: f32| Rect::new(w - em(40.0) - i * em(TOOL_PITCH), em(6.0), em(28.0), em(28.0));
        let more = tool(0.0);
        let (share, downloads, aa) = if fold { (None, tool(1.0), more) } else { (Some(tool(1.0)), tool(2.0), tool(3.0)) };
        let tools_x = if fold { downloads.x() } else { aa.x() };
        let pw = em(if presence_label { PRESENCE_PILL } else { PRESENCE_ICON });
        let presence = Rect::new(tools_x - em(8.0) - pw, em(6.0), pw, em(28.0));
        let right = w - presence.x() + em(12.0) + if media { em(32.0) } else { 0.0 };
        let mut where_ = Rect::new(fx, em(6.0), (w - fx - right).max(em(160.0)), em(28.0));
        // On Start the field is the page's: a wide one under "Where to?"
        // (§15.1), and the toolbar keeps only its buttons.
        if self.on_start() {
            let col = em(START_COL).min(w - x0 - em(64.0)).max(em(200.0));
            where_ = Rect::new(x0 + (w - x0 - col) / 2.0, bar() + em(START_FIELD_Y), col, em(START_FIELD_H));
        }
        let keep = Rect::new(where_.x() + where_.w() - em(28.0), where_.y() + em(2.0), em(24.0), em(24.0));
        let mut y = bar() + 1.0;
        let page = self.store.trail().page();
        let find = (self.find.is_some() && page.is_some() && self.history.is_none()).then(|| {
            let r = Rect::new(x0, y, w - x0, em(FIND));
            y += em(FIND);
            r
        });
        let form = self.portal.as_ref().filter(|_| self.history.is_none()).map(|_| {
            let rows = self.form.as_ref().map_or(0.0, |f| f.rows().len().min(FORM_ROWS_SHOWN) as f32 + 1.0);
            let r = Rect::new(x0, y, w - x0, em(FORM_HEAD) + rows * em(FORM_ROW) + if rows > 0.0 { em(8.0) } else { 0.0 });
            y += r.h();
            r
        });
        let prompt = page.filter(|_| self.history.is_none()).and_then(|p| p.asked.as_ref()).map(|_| {
            let r = Rect::new(x0, y, w - x0, em(PROMPT));
            y += em(PROMPT);
            r
        });
        let status = (self.status_bar && page.is_some()).then(|| Rect::new(x0, h - em(STATUS), w - x0, em(STATUS)));
        let bottom = status.map_or(h, |s| s.y());
        let page = Rect::new(x0, y, w - x0, (bottom - y).max(0.0));
        let drop = self.editing.as_ref().and_then(|_| {
            let v = self.suggestions();
            (!v.is_empty()).then(|| Rect::new(where_.x(), where_.y() + where_.h() + 4.0, where_.w(), drop_height(&v)))
        });
        let popover = (self.popover && self.store.trail().page().is_some())
            .then(|| {
                let pw = em(POPOVER_W).min(w - where_.x() - 8.0);
                Rect::new(where_.x(), bar() + 4.0, pw, popover_height(self, pw))
            });
        let reading = (self.reading_open && self.store.trail().page().is_some())
            .then(|| Rect::new((aa.x() + aa.w() - em(READING_W)).max(x0 + 8.0), bar() + 4.0, em(READING_W), em(READING_H)));
        let presence_menu = (self.presence_open && self.store.trail().page().is_some())
            .then(|| Rect::new((presence.x() + presence.w() - em(PRESENCE_W)).max(x0 + 8.0), bar() + 4.0, em(PRESENCE_W), presence_height(self)));
        let sections_button = find.filter(|_| !self.sections.is_empty()).map(|f| Rect::new(f.x() + f.w() - em(140.0), f.y() + em(6.0), em(92.0), em(28.0)));
        let sections_menu = sections_button.filter(|_| self.sections_open).map(|b| {
            let n = self.sections_shown().len().max(1) as f32;
            Rect::new((b.x() + b.w() - em(SECTIONS_W)).max(x0 + 8.0), b.y() + b.h() + 4.0, em(SECTIONS_W), em(16.0) + n * em(SECTION_ROW))
        });
        let persona_chip = Rect::new(em(12.0), em(6.0), em(122.0), em(28.0));
        let persona_menu = (self.persona_open && rail > em(100.0)).then(|| {
            Rect::new(em(8.0), persona_chip.y() + persona_chip.h() + 4.0, em(PERSONA_W), em(PERSONA_ROW) * self.persona_rows().len() as f32 + em(16.0) + em(26.0))
        });
        let more_menu = self.more_open.then(|| {
            let n = self.more_rows_at(fold).len() as f32;
            Rect::new((more.x() + more.w() - em(MORE_W)).max(x0 + 8.0), bar() + 4.0, em(MORE_W), em(16.0) + n * em(MORE_ROW))
        });
        let downloads_menu = self.downloads_open.then(|| {
            let n = self.downloads.len().max(1) as f32;
            Rect::new((downloads.x() + downloads.w() - em(DOWNLOADS_W)).max(x0 + 8.0), bar() + 4.0, em(DOWNLOADS_W), em(40.0) + n * em(DOWNLOAD_ROW))
        });
        let wide = rail > em(RAIL_NARROW);
        let rail_find = wide.then(|| Rect::new(em(12.0), em(42.0), rail - em(24.0), em(28.0)));
        let all_history = wide.then(|| Rect::new(em(8.0), h - em(RAIL_FOOT) + em(4.0), rail - em(16.0), em(RAIL_FOOT) - em(8.0)));
        Geo { w, h, rail, x0, where_, prompt, page, status, drop, popover, find, reading, aa, media, presence, presence_menu, persona_chip, persona_menu, sections_menu, sections_button, form, keep, presence_label, downloads, share, more, downloads_menu, more_menu, rail_find, all_history }
    }

    // -- the Where field ---------------------------------------------------

    /// Click or Ctrl-L: the field becomes a text field holding the full
    /// address, selected (§5.3).
    pub fn begin_edit(&mut self) {
        let url = self.store.trail().page().map_or(String::new(), |p| p.url.clone());
        let mut edit = TextEdit::new(&url);
        edit.select_all();
        self.editing = Some(WhereEdit { edit, selected: None });
        self.popover = false;
    }

    pub fn end_edit(&mut self) {
        self.editing = None;
    }

    /// A key while the field is focused. Returns true when it was used.
    pub fn edit_key(&mut self, key: Key, m: Modifiers, chars: &str, now: f64) -> bool {
        let n = self.suggestions().len();
        let Some(e) = &mut self.editing else { return false };
        match key {
            Key::Escape => {
                self.end_edit();
                return true;
            }
            Key::Down if n > 0 => {
                e.selected = Some(e.selected.map_or(0, |s| (s + 1).min(n - 1)));
                return true;
            }
            Key::Up => {
                e.selected = e.selected.and_then(|s| s.checked_sub(1));
                return true;
            }
            _ => {}
        }
        match e.edit.key(key, m, chars) {
            KeyOutcome::Changed => {
                e.selected = None;
                true
            }
            KeyOutcome::Submit => {
                let (sel, text) = (e.selected, e.edit.text().to_string());
                match sel.and_then(|i| self.suggestions().get(i).cloned()) {
                    Some(s) => self.take_suggestion(&s, now),
                    None => self.enter(&text),
                }
                true
            }
            KeyOutcome::Copy(t) => {
                let _ = pergola::clipboard::copy_text(&t, false);
                true
            }
            // Paste needs the press's activation token (tabula.md), which
            // the window hook doesn't carry yet.
            KeyOutcome::Paste | KeyOutcome::Ignored => false,
        }
    }

    /// Enter on typed text: an address goes there, anything else is a
    /// search. This is the first byte that leaves the machine (D4).
    pub fn enter(&mut self, text: &str) {
        let url = match where_field::parse(text) {
            Some(Entry::Address(a)) => a,
            Some(Entry::Search(q)) => fake::search_url(&q),
            None => return,
        };
        self.end_edit();
        let spec = self.spec(&url);
        self.store.follow(spec);
    }

    pub fn take_suggestion(&mut self, s: &Suggestion, _now: f64) {
        self.end_edit();
        match &s.source {
            Source::Open { trail, page, .. } => {
                self.store.select(*trail);
                self.store.trail_mut().go(*page);
            }
            Source::Kept | Source::History => {
                let spec = self.spec(&s.url);
                self.store.follow(spec);
            }
            Source::Search => {
                let spec = self.spec(&fake::search_url(&s.title));
                self.store.follow(spec);
            }
        }
    }

    // -- the page ----------------------------------------------------------

    /// A link in the page: a plain click follows, Ctrl-click opens beside
    /// (§4.2), and a link to a file asks in the prompt row (§6.4).
    pub fn activate(&mut self, l: &Link, now: f64) {
        if let Some((file, size)) = &l.download {
            let origin = crate::trail::site(&l.url).to_string();
            self.ask(Prompt::Download { id: 0, file: file.clone(), size: size.clone(), origin, kind: "PDF".into() }, now);
            return;
        }
        let spec = fake::spec(&l.url);
        if self.mods.ctrl {
            self.store.trail_mut().open_beside(spec);
        } else {
            self.store.follow(spec);
        }
    }

    /// Whether the tier popover offers "Run this page live" (I6.1): live
    /// pages are on, the page on screen is an HTTPS page from navigatord,
    /// and it isn't running live already.
    pub fn offers_live(&self) -> bool {
        self.live_pages && self.live.is_some() && self.history.is_none() && self.live_run.is_none()
            && self.shown_url().is_some_and(|u| u.starts_with("https://"))
    }

    /// The popover's "Run this page live": ask in the prompt row, whose
    /// "Run live" is the press that counts (§6.4's 500 ms still and
    /// uncovered). Asking again after "Not now" is the person's choice.
    pub fn offer_live(&mut self, now: f64) -> bool {
        if !self.offers_live() { return false; }
        let Some(url) = self.shown_url() else { return false };
        let site = crate::trail::site(&url).to_string();
        self.popover = false;
        if let Some(page) = self.store.trail_mut().page_mut() {
            page.prompts.dismissed.retain(|k| k != "run-page-live");
            page.prompts.last_ms = None;
        }
        let recorded = !self.store.trail().jar().unrecorded();
        self.ask(Prompt::RunPageLive { site, recorded }, now)
    }

    /// navigatord said the page at `url` is of an origin the persona runs
    /// live every time (D8): it runs live now if it is the page on screen
    /// and nothing runs live yet. navigatord checks the origin and the jar
    /// again before it starts anything.
    pub fn live_remembered(&mut self, url: &str) -> bool {
        let Some(o) = origin_of(url).map(str::to_string) else { return false };
        self.live_remembered.retain(|x| *x != o);
        self.live_remembered.push(o);
        if self.live_remembered.len() > LIVE_REMEMBERED_MAX { self.live_remembered.remove(0); }
        let (Some(key), Some(shown)) = (self.shown_key(), self.shown_url()) else { return false };
        if shown != url || !self.offers_live() || self.store.trail().jar().unrecorded()
            || !matches!(&self.live, Some(LivePage::Shown(u)) if u == url) {
            return false;
        }
        self.live_run = Some(LiveRun { key, url: url.to_string(), origin: None, entries: vec![key], refused: Vec::new() });
        let jar = self.store.trail().jar();
        self.outbox.push(Out::ContinueLive { url: url.to_string(), jar });
        true
    }

    /// Whether the page on screen is of an origin run live every time.
    pub fn shown_remembered(&self) -> bool {
        self.live_pages && self.history.is_none() && !self.store.trail().jar().unrecorded()
            && self.shown_url().is_some_and(|u| origin_of(&u).is_some_and(|o| self.live_remembered.iter().any(|x| x == o)))
    }

    /// The popover's "Ask before running live" (D8): the page's origin is
    /// no longer run live unasked. What runs live now goes on.
    pub fn forget_live(&mut self) {
        let Some(url) = self.shown_url() else { return };
        let Some(o) = origin_of(&url).map(str::to_string) else { return };
        self.popover = false;
        self.live_remembered.retain(|x| *x != o);
        self.outbox.push(Out::ForgetLive { url });
    }

    /// The popover's "Stop running live", or leaving the page: the
    /// document lane shows again.
    pub fn stop_live(&mut self) {
        let Some(run) = self.live_run.take() else { return };
        self.popover = false;
        self.set_live_whole(run.key, false);
        self.outbox.push(Out::StopLive { url: run.url });
    }

    /// The live page asked to go to `url` (I6.2; navigatord checked it):
    /// a new trail entry, like a followed link. It runs live too once
    /// navigatord shows it, if it is the same origin ([`Self::live_follow`]).
    pub fn live_navigated(&mut self, from: &str, url: &str) -> bool {
        let shown = self.shown_key();
        let Some(run) = self.live_run.as_ref().filter(|r| r.url == from && r.origin.is_some()) else { return false };
        if self.history.is_some() || shown != Some(run.key) { return false; }
        self.follow_link(url, false);
        let next = self.shown_key();
        if let (Some(run), Some(next)) = (self.live_run.as_mut(), next) { run.entries.push(next); }
        true
    }

    /// The live page asked for a new window (I6.2): an entry beside it in
    /// the trail, opened in the document lane when the person goes there.
    /// The page stays on screen and keeps running.
    pub fn live_opened_beside(&mut self, from: &str, url: &str) -> bool {
        let shown = self.shown_key();
        let Some(run) = self.live_run.as_ref().filter(|r| r.url == from && r.origin.is_some()) else { return false };
        if self.history.is_some() || shown != Some(run.key) { return false; }
        self.follow_link(url, true);
        true
    }

    /// The live page changed its own address (`history.pushState`, I6.2;
    /// navigatord checked the origin): the same engine, a new trail entry
    /// with nothing to open, or the entry just before or after it when the
    /// page went back to that address itself.
    pub fn live_url_changed(&mut self, from: &str, url: &str) -> bool {
        let shown = self.shown_key();
        let Some(run) = self.live_run.clone().filter(|r| r.url == from && r.origin.is_some()) else { return false };
        if self.history.is_some() || shown != Some(run.key) { return false; }
        if origin_of(url).is_none() || origin_of(url) != run.origin.as_deref().and_then(origin_of) { return false; }
        let (tid, here) = run.key;
        let t = self.store.trail();
        if t.pages[here].url == url { return false; }
        let mine = |id: PageId| run.entries.contains(&(tid, id)) && t.pages[id].url == url;
        let to = match t.pages[here].parent.filter(|&p| mine(p)) {
            Some(p) => Some(p),
            None => t.pages[here].children.iter().rev().copied().find(|&c| mine(c)),
        };
        self.set_live_whole(run.key, false);
        let key = match to {
            Some(id) => { self.store.trail_mut().go(id); (tid, id) }
            None => {
                let spec = self.spec(url);
                (tid, self.store.follow(spec))
            }
        };
        self.set_live_whole(key, true);
        if let Some(run) = self.live_run.as_mut() {
            run.key = key;
            if !run.entries.contains(&key) { run.entries.push(key); }
        }
        true
    }

    /// The page the live engine shows (I6.2: an entry it made by changing
    /// its address has no page of its own to open).
    pub fn live_route(&self, key: (u64, PageId)) -> bool {
        self.history.is_none() && self.live_run.as_ref().is_some_and(|r| r.key == key && r.origin.is_some())
    }

    /// The page on screen changed while a page runs live: an entry of this
    /// run (the page the live page went to, or Back and Forward among its
    /// entries) runs live again once navigatord shows it at the same
    /// origin; anything else (another tab, another origin, a failed open)
    /// stops it. Returns whether anything changed.
    pub fn live_follow(&mut self) -> bool {
        let Some(run) = self.live_run.clone() else { return false };
        let shown = self.shown_key();
        if self.history.is_none() && shown == Some(run.key) { return false; }
        if let Some(to) = shown.filter(|k| self.history.is_none() && run.entries.contains(k)) {
            match &self.live {
                Some(LivePage::Loading(_)) => return false,
                Some(LivePage::Shown(u)) if origin_of(u).is_some() && origin_of(u) == run.origin.as_deref().and_then(origin_of) => {
                    let url = u.clone();
                    self.set_live_whole(run.key, false);
                    self.live_run = Some(LiveRun { key: to, url: url.clone(), ..run });
                    let jar = self.store.trail().jar();
                    self.outbox.push(Out::ContinueLive { url, jar });
                    return true;
                }
                _ => {}
            }
        }
        self.stop_live();
        true
    }

    /// navigatord said the page runs live (`origin`), or ended it (`None`).
    pub fn live_reported(&mut self, url: &str, origin: Option<String>, why: Option<String>) {
        let Some(run) = self.live_run.as_mut().filter(|r| r.url == url) else { return };
        let key = run.key;
        match origin {
            Some(o) => {
                run.origin = Some(o);
                self.set_live_whole(key, true);
            }
            None => {
                self.live_run = None;
                self.set_live_whole(key, false);
                self.last_note = Some(match why {
                    Some(w) => format!("the page stopped running live: {w}"),
                    None => "the page stopped running live".into(),
                });
            }
        }
    }

    /// The live page at `from` offered a file (I6.4): asked on its own
    /// entry, while it keeps running. False when it was not asked (the
    /// run is gone, or the prompt row won't take it), so the caller
    /// discards it.
    pub fn live_file(&mut self, from: &str, p: Prompt, now: f64) -> bool {
        let Some(key) = self.live_run.as_ref().filter(|r| r.url == from && r.origin.is_some()).map(|r| r.key) else { return false };
        self.ask_at(key, p, now)
    }

    /// The live page at `from` was refused something (I6.4): named in the
    /// popover, each once.
    pub fn live_refused(&mut self, from: &str, what: &str) -> bool {
        let Some(run) = self.live_run.as_mut().filter(|r| r.url == from) else { return false };
        if what.is_empty() || what.len() > navigator_control::REFUSED_MAX || what.chars().any(char::is_control) { return false; }
        if !run.refused.iter().any(|r| r == what) && run.refused.len() < LIVE_REFUSED_MAX {
            run.refused.push(what.to_string());
        }
        true
    }

    /// The live page at `from` asked to be printed (I6.4): its entry's
    /// document-lane rendering prints, as Ctrl-P does; only while it is
    /// the page on screen.
    pub fn live_print(&mut self, from: &str) -> bool {
        if !self.live_run.as_ref().is_some_and(|r| r.url == from && r.origin.is_some() && Some(r.key) == self.shown_key()) { return false; }
        let before = self.outbox.len();
        self.print();
        self.outbox.len() > before
    }

    /// Whether the live page at `from` is running and on screen: only it
    /// may copy or paste (I6.4).
    pub fn live_on_screen(&self, from: &str) -> bool {
        self.history.is_none() && self.live_run.as_ref().is_some_and(|r| r.url == from && r.origin.is_some() && Some(r.key) == self.shown_key())
    }

    /// The popover's line for what the live page on screen was refused.
    pub fn live_refusals(&self) -> Option<String> {
        let run = self.live_run.as_ref().filter(|r| Some(r.key) == self.shown_key() && !r.refused.is_empty())?;
        Some(format!("Refused here: {}.", run.refused.join("; ")))
    }

    fn set_live_whole(&mut self, key: (u64, PageId), on: bool) {
        if let Some(page) = self.store.page_by_key_mut(key.0, key.1) {
            page.live_whole = on;
        }
    }

    /// Ask on the page on screen.
    pub fn ask(&mut self, p: Prompt, now: f64) -> bool {
        let Some(key) = self.shown_key() else { return false };
        self.ask_at(key, p, now)
    }

    /// Ask on the page `key` (§6.4): one prompt shown at a time, newer ones
    /// waiting behind it (at most [`PROMPTS_QUEUED`]); a page can't ask
    /// faster than once a second, and a prompt dismissed on a page doesn't
    /// come back for it until it is loaded again. Returns whether it was
    /// taken.
    pub fn ask_at(&mut self, key: (u64, PageId), p: Prompt, now: f64) -> bool {
        let Some(page) = self.store.page_by_key_mut(key.0, key.1) else { return false };
        let k = p.key();
        let q = &mut page.prompts;
        let broker_notice = matches!(p, Prompt::WidgetFailed { .. });
        if !broker_notice && (q.dismissed.contains(&k) || q.last_ms.is_some_and(|t| now - t < PROMPT_EVERY_MS)) {
            return false;
        }
        if page.asked.as_ref().is_some_and(|a| a.prompt.key() == k) || q.queued.iter().any(|x| x.key() == k) {
            return false;
        }
        match page.asked {
            None => page.asked = Some(Asked { prompt: p, since_ms: now }),
            Some(_) if q.queued.len() < PROMPTS_QUEUED => q.queued.push(p),
            Some(_) => return false,
        }
        if !broker_notice { q.last_ms = Some(now); }
        true
    }

    /// The child names a source. The signed row takes its origin only from
    /// the broker's metadata for this specific open.
    pub fn widget_pressed(&mut self, key: (u64, PageId), sources: &[(String, String)], src: &str, island: u32, rect: [f32; 4], runnable: bool, now: f64) -> bool {
        if self.history.is_some() || self.shown_key() != Some(key) { return false; }
        let Some(page) = self.store.trail().page() else { return false };
        let replaced = page.asked.iter().map(|a| &a.prompt).chain(page.prompts.queued.iter())
            .any(|p| matches!(p, Prompt::Widget { key, src: old, .. } if *key != island || old != src));
        if replaced {
            self.widget_placement(key, None, None, now);
        }
        let Some((_, origin)) = sources.iter().find(|(s, _)| s == src) else { return replaced };
        self.ask_at(key, Prompt::Widget { runnable, key: island, src: src.into(), origin: origin.clone(), rect }, now) || replaced
    }

    /// Update only an existing widget row. Closing or hiding the page also
    /// removes queued rows, leaving unrelated questions in their original order.
    pub fn widget_placement(&mut self, page: (u64, PageId), island: Option<u32>, rect: Option<[f32; 4]>, now: f64) -> bool {
        let Some(page) = self.store.page_by_key_mut(page.0, page.1) else { return false };
        let matches = |p: &Prompt| matches!(p, Prompt::Widget { key, .. } | Prompt::WidgetUnload { key, .. } | Prompt::WidgetFailed { key, .. } if island.is_none_or(|id| id == *key));
        if let Some(new_rect) = rect {
            let rows = page.asked.iter_mut().map(|a| &mut a.prompt).chain(page.prompts.queued.iter_mut());
            for row in rows {
                if matches(row) {
                    if let Prompt::Widget { rect, .. } = row {
                        *rect = new_rect;
                        return true;
                    }
                }
            }
            return false;
        }
        let before = page.prompts.queued.len();
        page.prompts.queued.retain(|p| !matches(p));
        if page.asked.as_ref().is_some_and(|a| matches(&a.prompt)) {
            page.asked = (!page.prompts.queued.is_empty()).then(|| Asked {
                prompt: page.prompts.queued.remove(0), since_ms: now,
            });
            return true;
        }
        before != page.prompts.queued.len()
    }

    pub fn clear_unload(&mut self, key: (u64, PageId), start: navigator_control::Press, query: Option<u32>, now: f64) {
        let Some(page) = self.store.page_by_key_mut(key.0, key.1) else { return };
        let matches = |p: &Prompt| matches!(p, Prompt::WidgetUnload { start: id, query: q, .. } if *id == start && query.is_none_or(|n| n == *q));
        page.prompts.queued.retain(|p| !matches(p));
        if page.asked.as_ref().is_some_and(|a| matches(&a.prompt)) {
            page.asked = (!page.prompts.queued.is_empty()).then(|| Asked { prompt: page.prompts.queued.remove(0), since_ms: now });
        }
    }

    /// Whether something of the UI's own covers the prompt row (the
    /// suggestion list, the tier popover).
    pub fn prompt_obscured(&self) -> bool {
        let g = self.geo();
        let Some(r) = g.prompt else { return false };
        let over = |o: Option<Rect>| o.is_some_and(|o| o.x() < r.x() + r.w() && r.x() < o.x() + o.w() && o.y() < r.y() + r.h() && r.y() < o.y() + o.h());
        over(g.drop) || over(g.popover) || over(g.reading) || over(g.presence_menu) || over(g.sections_menu) || over(g.persona_menu)
    }

    /// Called every frame: a prompt row that moved, or is covered, starts
    /// its 500 ms again. Returns whether anything changed.
    pub fn settle(&mut self, now: f64) -> bool {
        // Find belongs to the page it was opened on.
        if self.find.as_ref().is_some_and(|f| f.key != self.shown_key()) {
            self.find = None;
        }
        let at = self.geo().prompt;
        let covered = self.prompt_obscured();
        let moved = at != self.prompt_at;
        self.prompt_at = at;
        if !(moved || covered) {
            return false;
        }
        match self.store.trail_mut().page_mut().and_then(|p| p.asked.as_mut()) {
            Some(a) if a.since_ms < now => {
                a.since_ms = now;
                true
            }
            _ => moved,
        }
    }

    /// A press on a prompt's answer. It counts only once the row has been
    /// still and uncovered for 500 ms (§6.4); returns whether it counted.
    pub fn answer(&mut self, i: usize, now: f64) -> bool {
        self.answer_pressed(i, now, None)
    }

    /// [`Shell::answer`] with frescod's token for the press, passed on
    /// with what navigatord does.
    pub fn answer_pressed(&mut self, i: usize, now: f64, press: Option<navigator_control::Press>) -> bool {
        let press = press.unwrap_or_default();
        if self.prompt_obscured() {
            return false;
        }
        let key = self.shown_key();
        let Some(page) = self.store.trail_mut().page_mut() else { return false };
        let Some(a) = &page.asked else { return false };
        if now - a.since_ms < PROMPT_STILL_MS {
            return false;
        }
        let Some(&what) = a.prompt.answers().get(i) else { return false };
        let prompt = a.prompt.clone();
        // The next one waiting gets its own 500 ms.
        page.asked = (!page.prompts.queued.is_empty()).then(|| Asked { prompt: page.prompts.queued.remove(0), since_ms: now });
        if matches!(what, "Cancel" | "Not now") {
            page.prompts.dismissed.push(prompt.key());
        }
        self.last_note = Some(format!("prompt answered: {what} ({prompt:?})"));
        let live = self.live.is_some();
        let sent = self.outbox.len();
        match (&prompt, what) {
            (Prompt::NotPrivate { .. }, "Go back") => {
                self.store.trail_mut().back();
            }
            (Prompt::Download { id, file, size, origin, .. }, "Save") if live => {
                self.outbox.push(Out::Save { id: *id, press });
                self.note_download(*id, file, size, origin, DownloadState::Saving);
            }
            (Prompt::Download { id, .. }, "Cancel") if live => self.outbox.push(Out::Discard { id: *id }),
            (Prompt::Download { id, file, size, origin, .. }, "Open") if live => {
                self.outbox.push(Out::OpenFile { id: *id, press });
                self.note_download(*id, file, size, origin, DownloadState::Opening);
            }
            (Prompt::Handoff { url, .. } | Prompt::OpenApp { url, .. }, "Open") if live => {
                self.outbox.push(Out::Hand { url: url.clone(), how: navigator_control::Hand::Open, press })
            }
            (Prompt::Widget { runnable: true, key: island, src, .. }, "Start") if live => {
                if let Some(page) = key {
                    self.outbox.push(Out::StartWidget { page, key: *island, src: src.clone(), press });
                }
            }
            (Prompt::WidgetUnload { start, query, .. }, "Leave" | "Stay") if live => {
                self.outbox.push(Out::AnswerUnload { start: *start, query: *query, leave: what == "Leave", press });
            }
            (Prompt::RunPageLive { recorded, .. }, "Run live" | "Always for this site") if live && self.live_pages => {
                if let (Some(url), Some(key)) = (self.shown_url(), key) {
                    self.live_run = Some(LiveRun { key, url: url.clone(), origin: None, entries: vec![key], refused: Vec::new() });
                    let jar = self.store.trail().jar();
                    let remember = *recorded && what == "Always for this site" && !jar.unrecorded();
                    self.outbox.push(Out::RunPageLive { url, press, jar, remember });
                }
            }
            (Prompt::RunLive, "Run live") if live => {
                if let Some(url) = self.shown_url() {
                    self.outbox.push(Out::RunLive { url, jar: self.store.trail().jar(), press });
                }
            }
            (Prompt::Stored { .. }, "Fetch the current version") => self.reload(),
            _ => {}
        }
        if matches!(self.outbox.last(), Some(Out::RunLive { .. } | Out::RunPageLive { .. } | Out::Save { .. } | Out::OpenFile { .. } | Out::Hand { .. })) && self.outbox.len() > sent {
            self.answered = key.map(|k| (k, prompt));
        }
        true
    }

    /// frescod didn't count the press on the last answer (something was
    /// over the row, or it had just moved): the question comes back, in
    /// front, with its own 500 ms.
    pub fn press_refused(&mut self, why: &str, now: f64) {
        self.last_note = Some(format!("the press didn't count: {why}"));
        let Some((key, prompt)) = self.answered.take() else { return };
        let Some(page) = self.store.page_by_key_mut(key.0, key.1) else { return };
        if let Some(a) = page.asked.take() {
            page.prompts.queued.insert(0, a.prompt);
            page.prompts.queued.truncate(PROMPTS_QUEUED);
        }
        page.asked = Some(Asked { prompt, since_ms: now });
    }

    /// navigatord decided a link is for another program: ask first.
    /// `about`: for an `atrium-app:` link, what navigatord learned of the
    /// app from the package catalog (§6.4).
    pub fn handoff(&mut self, url: &str, scheme: &str, about: Option<navigator_control::AppAbout>, now: f64) {
        let p = if scheme == "atrium-app" {
            let name = url.trim_start_matches("atrium-app:").trim_start_matches("//");
            let name = name.split(['/', '?', '#']).next().unwrap_or("").to_string();
            let p = Prompt::OpenApp { url: url.into(), name, about };
            eprintln!("navigator-ui: asking: {}", crate::ax::prompt_text(&p));
            p
        } else {
            Prompt::Handoff { url: url.into(), scheme: scheme.into() }
        };
        self.ask(p, now);
    }

    /// The stop page's Go back (§6.5: no click-through, D10).
    pub fn leave_stopped(&mut self) {
        let t = self.store.trail_mut();
        if t.page().is_some_and(|p| p.facts.light == Light::Red) && !t.back() {
            self.store.put_away(crate::clock::now_s());
        }
    }

    /// Presence (§6.2): let the page's site remember you in this trail's
    /// jar, or forget it there. Either way the page opens again (live: the
    /// page child that showed it no longer matches), with or without the
    /// jar's cookies; forgetting also has navigatord tell Arca to destroy
    /// that site's jar.
    pub fn toggle_presence(&mut self) {
        let Some(p) = self.store.trail().page() else { return };
        let url = p.url.clone();
        let site = crate::trail::site(&url).to_string();
        let on = !self.store.knows(&site);
        self.store.set_knows(&site, on);
        if !on && self.live.is_some() {
            self.outbox.push(Out::Forget { jar: self.store.trail().jar(), url });
        }
        self.last_note = Some(if on { format!("{site} may remember you in this trail's jar") } else { format!("{site} forgotten in this trail's jar") });
    }

    /// The presence control pressed: its menu opens (§6.2). On a site
    /// that knows you, navigatord is asked which accounts Arca holds for it.
    /// "Open with…" (the toolbar's button): the shown page's address to
    /// Monstrator with the press, whose sheet offers the apps that take it
    /// and the person's other personas (personas.md §6).
    pub fn open_with(&mut self, press: Option<navigator_control::Press>) -> bool {
        let Some(LivePage::Shown(u)) = self.live.as_ref() else { return false };
        let url = u.clone();
        self.outbox.push(Out::Hand { url, how: navigator_control::Hand::With, press: press.unwrap_or([0; 16]) });
        true
    }

    /// The rail's persona chip: with other personas, its menu; without,
    /// it switches new trails between the window's persona and Unrecorded.
    pub fn press_persona_chip(&mut self) {
        if self.other_personas.is_empty() {
            self.store.persona = match self.store.persona {
                crate::trail::Persona::Personal => crate::trail::Persona::Unrecorded,
                crate::trail::Persona::Unrecorded => crate::trail::Persona::Personal,
            };
            return;
        }
        self.persona_open = !self.persona_open;
        if self.persona_open {
            self.rail_collapsed = false;
            self.popover = false;
            self.reading_open = false;
            self.presence_open = false;
            self.sections_open = false;
        }
    }

    /// The menu's rows: new trails in this window's persona or
    /// Unrecorded (`None`, with whether it is the one now), then a window
    /// in each other persona (`Some(name)`).
    pub fn persona_rows(&self) -> Vec<(Option<String>, String, bool)> {
        let here = self.window_persona.clone().unwrap_or_else(|| "Personal".into());
        let unrec = self.store.persona == crate::trail::Persona::Unrecorded;
        let mut v = vec![(None, format!("New trails: {here}"), !unrec), (None, "New trails: Unrecorded".to_string(), unrec)];
        for p in &self.other_personas {
            v.push((Some(p.clone()), format!("New {} window", fit_persona(p).unwrap_or_default()), false));
        }
        v
    }

    /// A row of the persona menu chosen; `press` for a New window.
    pub fn choose_persona_row(&mut self, i: usize, press: Option<navigator_control::Press>) {
        let Some((to, _, _)) = self.persona_rows().get(i).cloned() else { return };
        match to {
            None => {
                self.persona_open = false;
                self.store.persona = if i == 0 { crate::trail::Persona::Personal } else { crate::trail::Persona::Unrecorded };
            }
            // The menu stays up until the answer: the press was in its
            // window, and portcullisd spends it only while that window
            // still has the focus.
            Some(p) => {
                self.last_note = Some(format!("new window: asked for {p}"));
                self.outbox.push(Out::NewWindow { persona: p, press: press.unwrap_or([0; 16]) });
            }
        }
    }

    pub fn toggle_presence_menu(&mut self) {
        if self.presence_open {
            self.presence_open = false;
            return;
        }
        let Some(p) = self.store.trail().page() else { return };
        let url = p.url.clone();
        let site = crate::trail::site(&url).to_string();
        self.popover = false;
        self.reading_open = false;
        self.more_open = false;
        self.downloads_open = false;
        self.presence_open = true;
        self.accounts = None;
        if self.store.knows(&site) {
            self.accounts = Some((site, if self.live.is_some() { Accounts::Asking } else { Accounts::Unknown("no Arca here".into()) }));
            if self.live.is_some() {
                self.outbox.push(Out::Accounts { url });
            }
        }
    }

    /// The menu's "Let this site remember me", or "Forget this site".
    pub fn presence_choose(&mut self, remember: bool) {
        self.presence_open = false;
        let knows = self.store.trail().page().is_some_and(|p| self.store.knows(crate::trail::site(&p.url)));
        if knows != remember {
            self.toggle_presence();
        }
    }

    /// The menu's "Sign out here" (§6.2): the session cookies of the
    /// page's site in this trail's jar end, and the page opens again
    /// without them. The site still knows you (cookies with an expiry
    /// stay); "Forget this site" is for all of it.
    pub fn sign_out_here(&mut self) {
        self.presence_open = false;
        let Some(p) = self.store.trail().page() else { return };
        let url = p.url.clone();
        let site = crate::trail::site(&url).to_string();
        if !self.store.knows(&site) {
            return;
        }
        if self.live.is_some() {
            self.outbox.push(Out::SignOut { jar: self.store.trail().jar(), url });
            self.reload();
        }
        self.last_note = Some(format!("signing out of {site} here"));
    }

    /// The reader pressed a consent wall's control (§6.2, legacy-web
    /// §5.4.1e) and Arca kept its answer in this trail's jar: the site
    /// knows you here now, and the page opens again with its cookies.
    pub fn consented(&mut self, url: &str) {
        let Some(p) = self.store.trail().page() else { return };
        let site = crate::trail::site(url).to_string();
        if crate::trail::site(&p.url) != site {
            return;
        }
        self.store.set_knows(&site, true);
        self.reload();
        self.last_note = Some(format!("{site} keeps your answer in this trail's jar"));
    }

    /// navigatord's answer to `Accounts`, if it is for the open menu.
    pub fn accounts_answer(&mut self, url: &str, accounts: Vec<navigator_control::Account>, why: Option<String>) {
        let site = crate::trail::site(url);
        if let Some((s, a)) = &mut self.accounts {
            if s == site && *a == Accounts::Asking {
                *a = match why {
                    Some(w) => Accounts::Unknown(w),
                    None => Accounts::Known(accounts),
                };
            }
        }
    }

    /// "Make its own trail" on a branch row of the current trail (§4.2).
    pub fn make_own_trail(&mut self, page: PageId) {
        let Some(title) = self.store.trail().pages.get(page).map(|p| p.title.clone()) else { return };
        match self.store.make_own_trail(self.store.current, page) {
            Some(keys) => {
                self.last_note = Some(format!("\u{201c}{title}\u{201d} is its own trail now"));
                self.outbox.push(Out::Moved { keys });
            }
            None => self.last_note = Some("the trail's first page can't leave it".into()),
        }
    }

    pub fn toggle_mute(&mut self, trail: usize) {
        if let Some(t) = self.store.trails.get_mut(trail) {
            t.muted = !t.muted;
        }
    }

    pub fn new_trail(&mut self, unrecorded: bool) {
        self.store.new_trail(unrecorded);
        self.popover = false;
        self.begin_edit();
    }

    /// A link another app opened here (opening-links.md §5): a trail of
    /// its own, recorded, showing it. Only a web address the Where field
    /// would take as one; anything else is dropped.
    /// `from`: the person sent it from another of their personas. Into a
    /// named persona's window it opens anonymous (personas.md §6): a
    /// phishing link sent into Work meets no Work sign-in.
    pub fn incoming(&mut self, url: &str, from: Option<&str>) -> bool {
        let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            return false;
        }
        let Some(Entry::Address(a)) = where_field::parse(url) else { return false };
        self.store.new_trail(false);
        if let (Some(f), Some(_)) = (from, &self.window_persona) {
            self.store.trail_mut().anonymous = true;
            let mut c = f.chars();
            let f = c.next().map(|x| x.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
            self.last_note = Some(format!("sent from {f}: sites in this trail don't know you"));
        }
        self.popover = false;
        self.end_edit();
        let spec = self.spec(&a);
        self.store.follow(spec);
        true
    }

    /// Ctrl-P: only a page shown in its child prints, once at a time.
    fn print(&mut self) {
        if self.printing {
            return;
        }
        if matches!(self.live, Some(LivePage::Shown(_))) {
            self.printing = true;
            self.outbox.push(Out::Print);
        } else {
            self.last_note = Some("nothing to print here".into());
        }
    }

    pub fn apply(&mut self, a: Action, _now: f64) {
        self.persona_open = a == Action::NewWindow && !self.persona_open && !self.other_personas.is_empty();
        self.popover = false;
        self.reading_open = false;
        self.presence_open = false;
        if !matches!(a, Action::History | Action::ToggleStatus | Action::ToggleRail | Action::NextRegion | Action::PrevRegion | Action::NewWindow) {
            self.history = None;
        }
        match a {
            Action::FocusWhere => self.begin_edit(),
            Action::NewTrail => self.new_trail(false),
            Action::NewUnrecorded => self.new_trail(true),
            Action::NewWindow => {
                if self.persona_open {
                    self.rail_collapsed = false;
                }
            }
            Action::PutAway => {
                self.end_edit();
                self.store.put_away(crate::clock::now_s());
                if self.store.trail().current.is_none() {
                    self.begin_edit();
                }
            }
            Action::BringBack => {
                if self.store.bring_back(0) {
                    self.end_edit();
                }
            }
            Action::Back => {
                self.end_edit();
                self.store.trail_mut().back();
            }
            Action::Forward => {
                self.end_edit();
                self.store.trail_mut().forward();
            }
            Action::NextTrail | Action::PrevTrail => {
                self.end_edit();
                self.store.cycle(a == Action::NextTrail);
                self.focus_start();
            }
            Action::Nth(n) => {
                self.end_edit();
                self.store.nth(n);
                self.focus_start();
            }
            Action::ToggleRail => self.rail_collapsed = !self.rail_collapsed,
            Action::Keep => self.store.toggle_keep(),
            Action::Mute => {
                let c = self.store.current;
                self.toggle_mute(c);
            }
            Action::ToggleStatus => self.status_bar = !self.status_bar,
            Action::Find => self.open_find(),
            Action::History => {
                if self.history.take().is_none() {
                    self.open_history();
                }
            }
            Action::Reload => self.reload(),
            Action::Print => self.print(),
            Action::NextRegion | Action::PrevRegion => {
                let from = if self.editing.is_some() {
                    Some(Region::Toolbar)
                } else if self.rail_focus {
                    Some(Region::Rail)
                } else {
                    None
                };
                self.move_region(from, a == Action::NextRegion);
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                let mut r = self.store.reading();
                r.zoom_64 = match a {
                    Action::ZoomIn => crate::trail::ReadingPrefs::step_zoom(r.zoom_64, true),
                    Action::ZoomOut => crate::trail::ReadingPrefs::step_zoom(r.zoom_64, false),
                    _ => 64,
                };
                self.store.set_reading(r);
            }
        }
    }

    // -- F6 (§9) -------------------------------------------------------------

    /// F6 from the page (handed back by the child): the region after it.
    pub fn region_from_page(&mut self, forward: bool) {
        self.popover = false;
        self.reading_open = false;
        self.presence_open = false;
        self.move_region(Some(Region::Page), forward);
    }

    /// The keys go from `from` (`None`: the chrome, nothing in it focused,
    /// counts as before the rail) to the next region round rail, toolbar,
    /// page; the page only while one is shown (not Start, a stop page or
    /// the history view).
    fn move_region(&mut self, from: Option<Region>, forward: bool) {
        const ORDER: [Region; 3] = [Region::Rail, Region::Toolbar, Region::Page];
        let page = self.live_page_shown() && self.history.is_none();
        let at = match from {
            Some(r) => ORDER.iter().position(|&x| x == r).unwrap(),
            None if forward => 2,
            None => 0,
        };
        let mut i = at;
        let to = loop {
            i = if forward { (i + 1) % 3 } else { (i + 2) % 3 };
            if ORDER[i] != Region::Page || page {
                break ORDER[i];
            }
        };
        self.rail_focus = to == Region::Rail;
        match to {
            Region::Toolbar => {
                if self.editing.is_none() {
                    self.begin_edit();
                }
            }
            Region::Rail => self.end_edit(),
            Region::Page => {
                self.end_edit();
                self.focus_page = true;
            }
        }
        self.last_note = Some(format!("F6: the keys are in the {}", match to { Region::Rail => "rail", Region::Toolbar => "toolbar", Region::Page => "page" }));
    }

    /// A page the person can give the keys to is on screen.
    fn live_page_shown(&self) -> bool {
        self.store.trail().page().is_some() && !matches!(self.live, Some(LivePage::LiveOffer) | Some(LivePage::Failed { .. }) | Some(LivePage::File { .. }))
    }

    /// A key while the rail has the keys: Up and Down go through the
    /// trails in the rail's order, Enter or Escape leaves the rail.
    pub fn rail_key(&mut self, key: Key) -> bool {
        match key {
            Key::Up | Key::Down => {
                let order = self.store.rail_order();
                let at = order.iter().position(|&i| i == self.store.current).unwrap_or(0);
                let to = if key == Key::Down { (at + 1).min(order.len().saturating_sub(1)) } else { at.saturating_sub(1) };
                if let Some(&i) = order.get(to) {
                    // Not `show_trail`: a trail on Start would take the
                    // keys into its Where field.
                    self.popover = false;
                    self.store.select(i);
                }
            }
            Key::Enter | Key::Escape => self.rail_focus = false,
            _ => return false,
        }
        true
    }

    // -- Escape ------------------------------------------------------------

    /// Escape closes what is on top: a pop-up, then the Where field, then
    /// the history view, then find. Returns whether it closed anything.
    pub fn escape(&mut self) -> bool {
        if self.popover || self.reading_open || self.presence_open || self.sections_open || self.persona_open || self.more_open || self.downloads_open {
            self.more_open = false;
            self.downloads_open = false;
            self.persona_open = false;
            self.popover = false;
            self.reading_open = false;
            self.presence_open = false;
            self.sections_open = false;
        } else if self.rail_focus {
            self.rail_focus = false;
        } else if self.editing.is_some() {
            self.end_edit();
        } else if let Some(h) = &mut self.history {
            if h.confirm.take().is_none() {
                self.history = None;
            }
        } else if self.find.is_some() {
            self.close_find();
        } else {
            return false;
        }
        true
    }

    // -- the toolbar's right end (§15.1) --------------------------------------

    /// What the presence pill says for `site`.
    pub fn presence(&self, site: &str) -> Presence {
        if !self.store.knows(site) {
            return Presence::NotKnown;
        }
        match &self.accounts {
            Some((s, Accounts::Known(v))) if s == site => v.first().map_or(Presence::Remembers, |a| Presence::SignedIn(a.name.clone())),
            _ => Presence::Remembers,
        }
    }

    fn close_popups(&mut self) {
        self.popover = false;
        self.reading_open = false;
        self.presence_open = false;
        self.persona_open = false;
        self.sections_open = false;
        self.more_open = false;
        self.downloads_open = false;
    }

    /// A file answered for, or an answer about one from navigatord.
    fn note_download(&mut self, id: u32, file: &str, size: &str, origin: &str, state: DownloadState) {
        match self.downloads.iter_mut().find(|d| d.id == id) {
            Some(d) => d.state = state,
            None => {
                if self.downloads.len() >= DOWNLOADS_KEPT {
                    self.downloads.remove(0);
                }
                self.downloads.push(Download { id, file: plain(file), size: plain(size), origin: plain(origin), state });
            }
        }
        self.downloads_new = !self.downloads_open;
    }

    /// navigatord's answer for download `id`; false when the list has no
    /// such download (answered before this run, or dropped from the list).
    pub fn download_done(&mut self, id: u32, state: DownloadState) -> bool {
        let Some(d) = self.downloads.iter_mut().find(|d| d.id == id) else { return false };
        d.state = state;
        self.downloads_new = !self.downloads_open;
        true
    }

    pub fn toggle_downloads(&mut self) {
        let open = !self.downloads_open;
        self.close_popups();
        self.end_edit();
        self.downloads_open = open;
        if open {
            self.downloads_new = false;
        }
    }

    pub fn toggle_more(&mut self) {
        let open = !self.more_open;
        self.close_popups();
        self.end_edit();
        self.more_open = open;
    }

    pub fn toggle_reading(&mut self) {
        let open = !self.reading_open;
        self.close_popups();
        self.reading_open = open && self.store.trail().page().is_some();
    }

    /// The More menu's rows: Reading and Share first when they are folded
    /// into it.
    pub fn more_rows(&self) -> Vec<MoreRow> {
        self.more_rows_at(self.w - self.rail_w() - 1.0 < em(BAR_TOOLS_FOLD))
    }

    fn more_rows_at(&self, fold: bool) -> Vec<MoreRow> {
        let mut v = if fold { vec![MoreRow::Reading, MoreRow::Share] } else { vec![] };
        v.push(MoreRow::OpenWith);
        if self.store.trail().page().is_some() {
            v.push(if self.store.trail().pinned { MoreRow::Unpin } else { MoreRow::Pin });
        }
        v.extend([MoreRow::Find, MoreRow::Print, MoreRow::History, MoreRow::StatusBar]);
        v
    }

    /// A press on a More row, with frescod's token for the press (Share
    /// and Open with spend it).
    pub fn press_more(&mut self, row: MoreRow, press: Option<navigator_control::Press>, now: f64) {
        self.more_open = false;
        match row {
            MoreRow::Reading => self.toggle_reading(),
            MoreRow::Share => {
                self.share(press);
            }
            MoreRow::OpenWith => {
                if self.open_with(press) {
                    self.last_note = Some("open with: asked Monstrator".into());
                } else {
                    self.last_note = Some("open with: no page shown".into());
                }
            }
            MoreRow::Pin | MoreRow::Unpin => {
                let i = self.store.current;
                if !self.store.toggle_pin(i) {
                    self.last_note = Some(format!("pin: at most {} trails are pinned", crate::trail::PINNED_MAX));
                }
            }
            MoreRow::Find => self.apply(Action::Find, now),
            MoreRow::Print => self.apply(Action::Print, now),
            MoreRow::History => self.apply(Action::History, now),
            MoreRow::StatusBar => self.apply(Action::ToggleStatus, now),
        }
    }

    /// Share: the page shown, through the system's share sheet, which
    /// spends the press. One sheet at a time; needs a page and a press.
    pub fn share(&mut self, press: Option<navigator_control::Press>) -> bool {
        self.close_popups();
        let (Some(press), Some(LivePage::Shown(url)), false) = (press, self.live.as_ref(), self.sharing) else {
            self.last_note = Some("share: nothing to share now".into());
            return false;
        };
        let url = url.clone();
        let title = self.store.trail().page().map(|p| p.title.clone()).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| url.clone());
        self.sharing = true;
        self.outbox.push(Out::Share { url, title, press });
        true
    }

    /// The share ended (shared, closed, or failed: `note` says which).
    pub fn shared(&mut self, note: String) {
        self.sharing = false;
        self.last_note = Some(note);
    }

    // -- Find in trails (§15.1) ---------------------------------------------

    /// The query as the rail filters with it: lowercased, no controls.
    pub fn rail_query(&self) -> String {
        self.rail_find.as_ref().map_or(String::new(), |e| plain(e.text()).trim().to_lowercase())
    }

    /// A press on the field: it takes the keys.
    pub fn begin_rail_find(&mut self) {
        self.close_popups();
        self.end_edit();
        self.rail_focus = false;
        self.rail_find.get_or_insert_with(|| TextEdit::new(""));
        self.rail_find_focus = true;
    }

    /// The trails the rail shows, in its order, pinned ones apart.
    pub fn rail_trails(&self) -> (Vec<usize>, Vec<usize>) {
        let q = self.rail_query();
        let pinned = self.store.pinned_order().into_iter().filter(|&i| self.store.trails[i].matches(&q)).collect();
        let today = self.store.rail_order().into_iter().filter(|&i| !self.store.trails[i].pinned && self.store.trails[i].matches(&q)).collect();
        (pinned, today)
    }

    /// The trails put away that the rail shows (index into Earlier).
    pub fn rail_earlier(&self) -> Vec<usize> {
        let q = self.rail_query();
        (0..self.store.earlier.len()).filter(|&i| self.store.earlier[i].trail.matches(&q)).collect()
    }

    /// A key while Find in trails has the keys: typing filters the rail,
    /// Enter shows the first trail it matches, Escape clears the field.
    pub fn rail_find_key(&mut self, key: Key, m: Modifiers, chars: &str) -> bool {
        let Some(e) = &mut self.rail_find else { return false };
        match key {
            Key::Escape => {
                self.rail_find = None;
                self.rail_find_focus = false;
                return true;
            }
            Key::Enter => {
                let (pinned, today) = self.rail_trails();
                if let Some(&i) = today.first().or(pinned.first()) {
                    self.store.select(i);
                }
                self.rail_find_focus = false;
                return true;
            }
            _ => {}
        }
        match e.key(key, m, chars) {
            KeyOutcome::Changed | KeyOutcome::Submit => {
                // A query past what a person types is cut.
                if e.text().len() > RAIL_QUERY_MAX {
                    let t = limen::doc_viewer::cut(e.text(), RAIL_QUERY_MAX);
                    *e = TextEdit::new(&t);
                }
                true
            }
            KeyOutcome::Copy(t) => {
                let _ = pergola::clipboard::copy_text(&t, false);
                true
            }
            KeyOutcome::Paste | KeyOutcome::Ignored => false,
        }
    }

    // -- reading controls (§7) ---------------------------------------------

    /// Change the current site's reading choices (the Aa pop-up).
    pub fn reading_change(&mut self, f: impl FnOnce(&mut crate::trail::ReadingPrefs)) {
        let mut r = self.store.reading();
        f(&mut r);
        self.store.set_reading(r);
    }

    // -- find (§7) -----------------------------------------------------------

    /// Ctrl-F: the find bar, its text selected (what was looked for last
    /// on this page stays).
    pub fn open_find(&mut self) {
        if self.store.trail().page().is_none() {
            return;
        }
        self.end_edit();
        let key = self.shown_key();
        match &mut self.find {
            Some(f) if f.key == key => f.edit.select_all(),
            _ => self.find = Some(FindBar { edit: TextEdit::new(""), key, count: 0, current: 0, answered: true }),
        }
    }

    /// The sections the list shows: those whose heading has the find
    /// bar's text (all of them when none does, or it is empty), at most
    /// [`SECTIONS_SHOWN`].
    pub fn sections_shown(&self) -> Vec<(String, String)> {
        let q = self.find.as_ref().map(|f| f.edit.text().to_lowercase()).unwrap_or_default();
        let hit: Vec<_> = self.sections.iter().filter(|(_, t)| !q.is_empty() && t.to_lowercase().contains(&q)).cloned().collect();
        let v = if hit.is_empty() { self.sections.clone() } else { hit };
        v.into_iter().take(SECTIONS_SHOWN).collect()
    }

    /// The find bar's "Sections" button.
    pub fn toggle_sections(&mut self) {
        self.sections_open = !self.sections_open && !self.sections.is_empty();
    }

    /// A section chosen (§7): the page scrolls to it, the list closes.
    pub fn jump_to(&mut self, anchor: &str) {
        self.sections_open = false;
        if self.sections.iter().any(|(a, _)| a == anchor) {
            self.outbox.push(Out::Jump { anchor: anchor.to_string() });
        }
    }

    /// The page shown laid out with these sections.
    pub fn set_sections(&mut self, v: Vec<(String, String)>) {
        if self.sections != v {
            self.sections = v;
            if self.sections.is_empty() {
                self.sections_open = false;
            }
        }
    }

    /// Esc on the find bar: closed, and the page's mark cleared.
    pub fn close_find(&mut self) {
        self.sections_open = false;
        if self.find.take().is_some() {
            self.outbox.push(Out::Find { query: String::new(), step: 0 });
        }
    }

    // -- the sign-in window's form panel (atrium-net.md §8.1 P5) -----------

    /// The shown page's forms, as its child read them: the panel takes the
    /// first. The same form on the same page keeps what was typed.
    pub fn set_forms(&mut self, forms: Vec<limen::doc_viewer::Form>) {
        if self.portal.is_none() {
            return;
        }
        let key = self.shown_key();
        let Some(form) = forms.into_iter().next() else {
            self.form = None;
            return;
        };
        if self.form.as_ref().is_some_and(|f| f.key == key && f.form == form) {
            return;
        }
        let n = form.fields.len();
        let edits = form.fields.iter().map(|f| TextEdit::new(&f.value)).collect();
        let checked = form.fields.iter().map(|f| f.checked).collect();
        let mut p = FormPanel { key, form, edits, checked, focus: None, arca: None, waiting: None, vpn: window_vpn(), sent: 0 };
        p.focus = p.texts().first().copied();
        debug_assert_eq!(p.edits.len(), n);
        self.form = Some(p);
    }

    /// Where the form panel's rows and buttons are: (field, its row) for
    /// each row shown, and (field, its button) for each button, and the
    /// x where the controls start.
    pub fn form_layout(&self) -> (Vec<(usize, Rect)>, Vec<(usize, Rect)>, f32) {
        let (Some(r), Some(p)) = (self.geo().form, &self.form) else { return (vec![], vec![], 0.0) };
        let x = r.x() + em(16.0);
        let label_w = em(200.0).min(r.w() / 3.0);
        let cx = x + label_w + em(16.0);
        let mut y = r.y() + em(FORM_HEAD);
        let mut rows = Vec::new();
        let visible: Vec<_> = p.rows().into_iter().take(FORM_ROWS_SHOWN).collect();
        let credential = p.arca_field().filter(|i| visible.contains(i));
        for i in visible.into_iter().filter(|i| Some(*i) != credential) {
            rows.push((i, Rect::new(x, y, r.w() - em(32.0), em(FORM_ROW))));
            y += em(FORM_ROW);
        }
        let buttons = p.buttons().into_iter().enumerate()
            .map(|(k, i)| (i, Rect::new(cx + k as f32 * em(FORM_BUTTON_W + 12.0), y + em(4.0), em(FORM_BUTTON_W), em(28.0)))).collect();
        // The credential surface is another window. Keep it immediately
        // below the guarded chrome so it cannot cover the submit action.
        if let Some(i) = credential {
            rows.push((i, Rect::new(x, y + em(FORM_ROW), r.w() - em(32.0), em(FORM_ROW))));
        }
        (rows, buttons, cx)
    }

    /// A press on a field's row: a text field takes the keys, a checkbox
    /// turns over, a radio button is chosen from its group.
    pub fn form_field(&mut self, i: usize) {
        use limen::doc_viewer::FieldKind::*;
        let Some(p) = &mut self.form else { return };
        let Some(f) = p.form.fields.get(i) else { return };
        match f.kind {
            Password if p.arca_field() == Some(i) => {}
            Text | Password => p.focus = Some(i),
            Checkbox => p.checked[i] = !p.checked[i],
            Radio => {
                let name = f.name.clone();
                for (j, g) in p.form.fields.iter().enumerate() {
                    if g.kind == Radio && g.name == name {
                        p.checked[j] = j == i;
                    }
                }
            }
            Fixed | Scripted | Submit => {}
        }
    }

    /// A key while the panel has a text field: typed into it; Tab and
    /// Shift-Tab go through the text fields. Enter sends nothing: a form
    /// goes only on a press on one of its buttons.
    pub fn form_key(&mut self, key: Key, m: Modifiers, chars: &str) -> bool {
        let Some(p) = &mut self.form else { return false };
        let texts = p.texts();
        let Some(at) = p.focus.filter(|i| texts.contains(i)) else { return false };
        if key == Key::Tab {
            let k = texts.iter().position(|&i| i == at).unwrap_or(0);
            let n = texts.len();
            p.focus = Some(texts[if m.shift { (k + n - 1) % n } else { (k + 1) % n }]);
            return true;
        }
        match p.edits[at].key(key, m, chars) {
            KeyOutcome::Changed => {
                let t = p.edits[at].text().to_string();
                if t.len() > navigator_control::FORM_TEXT_MAX || t.chars().any(char::is_control) {
                    p.edits[at].set_text(&limen::doc_viewer::cut(&t, navigator_control::FORM_TEXT_MAX));
                }
                true
            }
            KeyOutcome::Copy(t) if p.form.fields[at].kind != limen::doc_viewer::FieldKind::Password => {
                let _ = pergola::clipboard::copy_text(&t, false);
                true
            }
            KeyOutcome::Submit | KeyOutcome::Copy(_) => true,
            KeyOutcome::Paste | KeyOutcome::Ignored => false,
        }
    }

    /// A press on button `i`: the form goes to navigatord as the page and
    /// the person filled it, with that button's own name and value.
    pub fn submit_form(&mut self, i: usize, press: Option<navigator_control::Press>) {
        use limen::doc_viewer::FieldKind::*;
        let (Some(p), Some(_)) = (&self.form, self.shown_url()) else { return };
        if p.form.fields.get(i).is_none_or(|f| f.kind != Submit) || p.key != self.shown_key() {
            return;
        }
        if p.arca_field().is_some() && p.arca.is_none() {
            // Arca holds what was typed as its field loses the keys, which
            // this press just did: the form waits for its handle.
            if let Some(f) = &mut self.form {
                f.waiting = Some((i, press, std::time::Instant::now() + ARCA_WAIT));
            }
            return;
        }
        self.send_form(i, press);
    }

    /// The form as filled, with button `i`, to navigatord.
    fn send_form(&mut self, i: usize, press: Option<navigator_control::Press>) {
        use limen::doc_viewer::FieldKind::*;
        let (Some(p), Some(page)) = (&self.form, self.shown_url()) else { return };
        let arca = p.arca_field();
        let mut fields = Vec::new();
        for (j, f) in p.form.fields.iter().enumerate() {
            if f.name.is_empty() {
                continue;
            }
            if Some(j) == arca {
                fields.push(navigator_control::FormField { name: f.name.clone(), value: String::new(), typed: true, arca: p.arca, scripted: false });
                continue;
            }
            let (value, typed) = match f.kind {
                Fixed => (f.value.clone(), false),
                Scripted => (f.value.clone(), false),
                Text | Password => (p.edits[j].text().to_string(), true),
                Checkbox | Radio if p.checked[j] => (f.value.clone(), false),
                Submit if j == i => (f.value.clone(), false),
                _ => continue,
            };
            fields.push(navigator_control::FormField { name: f.name.clone(), value, typed, arca: None, scripted: f.kind == Scripted });
        }
        let (label, action, post) = (p.form.fields[i].label.clone(), p.form.action.clone(), p.form.post);
        // Spent: a handle goes with one form.
        if let Some(f) = &mut self.form {
            f.arca = None;
            f.waiting = None;
            f.sent += 1;
        }
        self.last_note = Some(format!("sent \"{label}\""));
        self.outbox.push(Out::SubmitForm { page, jar: self.store.trail().jar(), action, post, fields, press: press.unwrap_or_default() });
    }

    /// Arca's field said what it holds: a handle, or nothing (cleared). A
    /// press that waited for it sends its form now.
    pub fn arca_handle(&mut self, h: Option<[u8; 16]>) {
        let Some(p) = &mut self.form else { return };
        p.arca = h;
        if let (Some(_), Some((i, press, _))) = (h, p.waiting.take()) {
            self.send_form(i, press);
        }
    }

    /// A press that waited past [`ARCA_WAIT`]: the field was empty, and
    /// the form goes without it.
    pub fn arca_tick(&mut self, now: std::time::Instant) {
        let Some(p) = &mut self.form else { return };
        if let Some((i, press, at)) = p.waiting {
            if now >= at {
                p.waiting = None;
                p.arca = None;
                self.send_form(i, press);
            }
        }
    }

    /// The page child's answer.
    pub fn found(&mut self, count: u32, current: u32) {
        if let Some(f) = &mut self.find {
            (f.count, f.current, f.answered) = (count, current, true);
        }
    }

    /// A key while the find bar has the keys: typing finds from the top
    /// of the view, Enter the next match, Shift-Enter the one before.
    pub fn find_key(&mut self, key: Key, m: Modifiers, chars: &str) -> bool {
        let Some(f) = &mut self.find else { return false };
        let step = match key {
            Key::Escape => {
                self.close_find();
                return true;
            }
            Key::Enter => if m.shift { -1 } else { 1 },
            _ => match f.edit.key(key, m, chars) {
                KeyOutcome::Changed => 0,
                KeyOutcome::Submit => if m.shift { -1 } else { 1 },
                KeyOutcome::Copy(t) => {
                    let _ = pergola::clipboard::copy_text(&t, false);
                    return true;
                }
                KeyOutcome::Paste | KeyOutcome::Ignored => return false,
            },
        };
        let query: String = f.edit.text().chars().filter(|c| !c.is_control()).collect();
        let query = limen::doc_viewer::cut(&query, limen::doc_viewer::QUERY_MAX);
        if query.is_empty() {
            (f.count, f.current) = (0, 0);
        }
        f.answered = query.is_empty();
        self.outbox.push(Out::Find { query, step });
        true
    }

    // -- the history view (§5.5) ---------------------------------------------

    pub fn open_history(&mut self) {
        self.end_edit();
        self.popover = false;
        self.reading_open = false;
        self.presence_open = false;
        self.history = Some(HistoryView { edit: TextEdit::new(""), selected: 0, confirm: None });
    }

    pub fn history_rows(&self) -> Vec<crate::history::Row> {
        self.history.as_ref().map_or_else(Vec::new, |h| crate::history::rows(&self.store, h.edit.text()))
    }

    /// A row pressed (or Enter on it): its page, in its trail. It opens
    /// as the version stored when it was seen, and the prompt row offers
    /// the current one (§5.5).
    pub fn open_history_row(&mut self, r: &crate::history::Row, now: f64) {
        self.history = None;
        if crate::history::open(&mut self.store, r) {
            self.last_note = Some(format!("history: opened {}", r.url));
            if let Some(p) = self.store.trail().page().filter(|p| p.saved.is_some()) {
                let at_s = p.at_s;
                self.ask(Prompt::Stored { at_s }, now);
            }
        }
    }

    /// Ask to clear history (§5.5): the confirmation shows what it takes.
    pub fn ask_clear(&mut self, what: crate::history::Clear) {
        let n = crate::history::count(&self.store, &what, crate::clock::now_s());
        if let Some(h) = &mut self.history {
            h.confirm = Some((what, n));
        }
    }

    /// The confirmation pressed: the pages go from the trail store, their
    /// page children close, and navigatord takes their stored versions out.
    pub fn confirm_clear(&mut self) {
        let Some((what, _)) = self.history.as_mut().and_then(|h| h.confirm.take()) else { return };
        let c = crate::history::clear(&mut self.store, &what, crate::clock::now_s());
        if let Some(h) = &mut self.history {
            h.selected = 0;
        }
        self.last_note = Some(format!("history: cleared {} pages {} ({} stored versions)", c.pages, what.label(), c.records.len()));
        if !c.keys.is_empty() || !c.records.is_empty() {
            self.outbox.push(Out::Clear { keys: c.keys, records: c.records });
        }
    }

    /// Reload, Ctrl-R, "Fetch the current version": the page is fetched
    /// again; its stored version is no longer the one it opens as, and
    /// what it was told no to may ask again (§6.4).
    pub fn reload(&mut self) {
        let Some(key) = self.shown_key() else { return };
        let Some(p) = self.store.trail_mut().page_mut() else { return };
        self.form = None;
        p.saved = None;
        p.asked = None;
        p.prompts = Default::default();
        self.last_note = Some(format!("reload: {}", p.url));
        self.outbox.push(Out::Reload { key });
    }

    /// A key in the history view: typing filters, Up and Down move,
    /// Enter opens, Escape closes.
    pub fn history_key(&mut self, key: Key, m: Modifiers, chars: &str) -> bool {
        let n = self.history_rows().len();
        let Some(h) = &mut self.history else { return false };
        if h.confirm.is_some() {
            match key {
                Key::Enter => self.confirm_clear(),
                Key::Escape => h.confirm = None,
                _ => {}
            }
            return true;
        }
        match key {
            Key::Escape => self.history = None,
            Key::Down => h.selected = (h.selected + 1).min(n.saturating_sub(1)),
            Key::Up => h.selected = h.selected.saturating_sub(1),
            Key::Enter => {
                let at = h.selected;
                if let Some(r) = self.history_rows().get(at).cloned() {
                    self.open_history_row(&r, crate::ui::now_ms());
                }
            }
            _ => match h.edit.key(key, m, chars) {
                KeyOutcome::Changed => h.selected = 0,
                KeyOutcome::Ignored | KeyOutcome::Paste => return false,
                _ => {}
            },
        }
        true
    }

    /// A trail on Start has its Where field focused.
    pub fn focus_start(&mut self) {
        if self.store.trail().current.is_none() && self.editing.is_none() {
            self.begin_edit();
        }
    }

    /// Whether Start is on screen: a trail with no page, not the history view.
    pub fn on_start(&self) -> bool {
        self.store.trail().page().is_none() && self.history.is_none()
    }

    /// From Start, go to trail `i` (a pinned card): the empty trail on
    /// Start closes rather than staying in the rail as "New trail".
    pub fn leave_start_for(&mut self, i: usize) {
        let Some(id) = self.store.trails.get(i).map(|t| t.id) else { return };
        if self.store.trail().current.is_none() && i != self.store.current {
            self.store.put_away(crate::clock::now_s());
        }
        if let Some(j) = self.store.trails.iter().position(|t| t.id == id) {
            self.show_trail(j);
        }
    }

    /// Show a trail from the rail.
    pub fn show_trail(&mut self, i: usize) {
        self.end_edit();
        self.popover = false;
        self.store.select(i);
        self.focus_start();
    }

    /// A press anywhere: it closes the field and the popover when it
    /// lands outside them (the press itself still goes on to what it hit).
    pub fn press_at(&mut self, x: f32, y: f32) {
        self.rail_focus = false;
        let g = self.geo();
        let inside = |r: Option<Rect>| r.is_some_and(|r| x >= r.x() && x < r.x() + r.w() && y >= r.y() && y < r.y() + r.h());
        if self.editing.is_some() && !inside(Some(g.where_)) && !inside(g.drop) {
            self.end_edit();
        }
        if self.popover && !inside(g.popover) && !inside(Some(chip_rect(&g))) {
            self.popover = false;
        }
        if self.reading_open && !inside(g.reading) && !inside(Some(g.aa)) {
            self.reading_open = false;
        }
        if self.presence_open && !inside(g.presence_menu) && !inside(Some(g.presence)) {
            self.presence_open = false;
        }
        if self.sections_open && !inside(g.sections_menu) && !inside(g.sections_button) {
            self.sections_open = false;
        }
        if self.persona_open && !inside(g.persona_menu) && !inside(Some(g.persona_chip)) {
            self.persona_open = false;
        }
        if self.more_open && !inside(g.more_menu) && !inside(Some(g.more)) {
            self.more_open = false;
        }
        if self.downloads_open && !inside(g.downloads_menu) && !inside(Some(g.downloads)) {
            self.downloads_open = false;
        }
        if self.rail_find_focus && !inside(g.rail_find) {
            self.rail_find_focus = false;
            if self.rail_query().is_empty() {
                self.rail_find = None;
            }
        }
    }
}

/// The tier chip's rect at the Where field's left end (approximate: the
/// chip's segments are measured when drawn; this covers the widest).
pub fn chip_rect(g: &Geo) -> Rect {
    Rect::new(g.where_.x(), g.where_.y(), em(220.0), g.where_.h())
}

/// The suggestion list's height for these rows (§5.3).
pub fn drop_height(v: &[Suggestion]) -> f32 {
    let mut h = em(8.0);
    let mut last: Option<u8> = None;
    for s in v {
        let group = match s.source {
            Source::Open { .. } => 0,
            Source::Kept | Source::History => 1,
            Source::Search => 2,
        };
        if last != Some(group) {
            h += em(if group == 2 { 9.0 } else { DROP_LABEL });
        }
        last = Some(group);
        h += em(DROP_ROW);
    }
    h + em(6.0)
}

/// The popover's height: two sections and the reasons (§6.5).
/// Measured from what it says, wrapped as `ui::popover` wraps it, so no
/// line falls out of the box at any text size.
pub fn popover_height(s: &Shell, w: f32) -> f32 {
    let Some(p) = s.store.trail().page() else { return 0.0 };
    let f = &*p.display_facts();
    let tw = w - em(32.0);
    let lines = |t: &str| wrap(t, tw).len() as f32;
    let site = crate::trail::site(&p.url);
    let mut n = lines(&popover_connection(f, site)) + lines(&crate::ui::tier_sentence(f, site)) + lines(popover_last(f));
    if let Some(r) = s.live_refusals() { n += lines(&r); }
    for o in &f.live_parts {
        n += lines(&popover_live_part(o));
    }
    for r in &f.reasons {
        n += lines(r);
    }
    // Top 14, heading 28, two section labels 16 each, two 6 gaps, lines 16, bottom 14;
    // the action row 6 + 32.
    let action = (6.0 + 32.0) * popover_actions(s).len() as f32;
    em(14.0 + 28.0 + 16.0 * 2.0 + 6.0 * 2.0 + 16.0 * n + 14.0 + action)
}

/// The popover's actions (I6.1): run live or stop, and for an origin run
/// live every time, asking again (D8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverAction { RunLive, Stop, Forget }

pub fn popover_action(s: &Shell) -> Option<PopoverAction> {
    if s.live_run.as_ref().is_some_and(|r| Some(r.key) == s.shown_key()) {
        Some(PopoverAction::Stop)
    } else if s.offers_live() {
        Some(PopoverAction::RunLive)
    } else {
        None
    }
}

pub fn popover_actions(s: &Shell) -> Vec<PopoverAction> {
    let mut all: Vec<PopoverAction> = popover_action(s).into_iter().collect();
    if s.shown_remembered() { all.push(PopoverAction::Forget); }
    all
}

/// The popover's connection line.
pub fn popover_connection(f: &Facts, site: &str) -> String {
    if f.insecure { "Not private. Anyone on this network can read or change this page.".to_string() } else { format!("{} to {site}.", f.connection) }
}

/// A live part's line in the popover.
pub fn popover_live_part(o: &str) -> String {
    format!("Live code runs in an embedded part from {o}.")
}

/// The popover's closing line.
pub fn popover_last(f: &Facts) -> &'static str {
    if f.light == Light::Green { "Green doesn't mean this site is who you think: check the name in the strip." } else { "This says what runs, not whether the site is honest." }
}

/// The small body text of the pop-ups (XS sans), broken into lines no
/// wider than `w` px at the text size.
pub fn wrap(s: &str, w: f32) -> Vec<String> {
    let st = pergola::TextStyle { family: pergola::theme::font::SANS.into(), size: pergola::theme::type_size::XS, weight: pergola::theme::Weight::Regular, color: pergola::Color::TRANSPARENT };
    let width = |t: &str| pergola::node::Node::measure_text(t, &st).w;
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split(' ') {
        let cand = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if width(&cand) > w && !line.is_empty() {
            out.push(std::mem::replace(&mut line, word.to_string()));
        } else {
            line = cand;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Scene;

    fn shell() -> Shell {
        Shell::new(fake::store(), 1280.0, 696.0)
    }

    #[test]
    fn a_live_trail_follows_what_navigatord_decides() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(false);
        s.enter("http://atlas.example/wiki/Atrium");
        assert_eq!(s.shown_url().as_deref(), Some("http://atlas.example/wiki/Atrium"));
        assert_eq!(s.store.trail().page().unwrap().title, "atlas.example", "the host until navigatord names it");
        assert_eq!(s.store.trail().page().unwrap().facts.light, Light::Amber, "http is not private");
        // Redirected to https, and named.
        let key = s.shown_key().unwrap();
        let saved = "0f".repeat(32);
        s.opened(key, "https://atlas.example/wiki/Atrium", "Atrium", false, &saved, true);
        let p = s.store.trail().page().unwrap();
        assert_eq!((p.url.as_str(), p.title.as_str(), p.facts.light), ("https://atlas.example/wiki/Atrium", "Atrium", Light::Green));
        assert_eq!(p.facts.tier, crate::trail::Tier::Replayed);
        assert_eq!(p.saved.as_deref(), Some(saved.as_str()), "the version navigatord keeps, for parking");
        // A press in the page goes to navigatord; its answer moves the trail.
        s.link_pressed("/wiki/Impluvium");
        assert_eq!(s.outbox, vec![Out::Activate { page: "https://atlas.example/wiki/Atrium".into(), href: "/wiki/Impluvium".into(), beside: false, jar: navigator_control::Jar::Personal }]);
        s.follow_link("https://atlas.example/wiki/Impluvium", false);
        assert_eq!(s.store.trail().pages.len(), 2);
        s.apply(Action::Back, 0.0);
        assert_eq!(s.shown_url().as_deref(), Some("https://atlas.example/wiki/Atrium"));
        s.apply(Action::Forward, 0.0);
        assert_eq!(s.shown_url().as_deref(), Some("https://atlas.example/wiki/Impluvium"));
        // Presence: letting the site remember you asks nothing of
        // navigatord until the page opens again; forgetting it does.
        s.outbox.clear();
        s.toggle_presence();
        let key = s.shown_key().unwrap();
        assert_eq!(s.store.presence_of(key.0, "https://atlas.example/x"), Some((navigator_control::Jar::Personal, true)));
        assert!(s.outbox.is_empty());
        s.toggle_presence();
        assert_eq!(s.outbox, vec![Out::Forget { jar: navigator_control::Jar::Personal, url: "https://atlas.example/wiki/Impluvium".into() }]);
        // An Unrecorded trail has its own jar, and the site doesn't know you there.
        s.toggle_presence();
        s.store.new_trail(true);
        s.enter("https://atlas.example/wiki/Atrium");
        let (t, _) = s.shown_key().unwrap();
        let (jar, knows) = s.store.presence_of(t, "https://atlas.example/").unwrap();
        assert!(matches!(jar, navigator_control::Jar::Unrecorded(j) if j != 0) && !knows);
    }

    #[test]
    fn a_vpn_sign_in_sends_arcas_handle_for_the_password_never_a_value() {
        use limen::doc_viewer::{Field, FieldKind::*, Form};
        let f = |kind, name: &str, label: &str, value: &str| Field { kind, name: name.into(), label: label.into(), value: value.into(), checked: false };
        let form = Form { action: "/login".into(), post: true, fields: vec![
            f(Text, "username", "Username", ""), f(Password, "password", "Password", ""), f(Submit, "login", "Sign In", "")] };
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(true);
        s.enter("https://idp.example/auth");
        s.portal = Some("the corp VPN".into());
        s.set_forms(vec![form.clone()]);
        s.form.as_mut().unwrap().vpn = true;
        let p = s.form.as_ref().unwrap();
        assert_eq!((p.arca_field(), p.texts()), (Some(1), vec![0]), "the password is Arca's field, not the panel's");
        let (rows, buttons, _) = s.form_layout();
        let credential = rows.iter().find(|(i, _)| *i == 1).unwrap().1;
        assert!(buttons.iter().all(|(_, b)| b.y() + b.h() <= credential.y()));
        s.form_field(1);
        assert_eq!(s.form.as_ref().unwrap().focus, Some(0), "the panel never takes its keys");
        for c in "alice".chars() {
            s.form_key(Key::Char, Modifiers::default(), &c.to_string());
        }
        // The press comes before Arca's handle (its field held what was
        // typed as it lost the keys): the form waits for it.
        s.submit_form(2, None);
        assert!(s.outbox.is_empty() && s.form.as_ref().unwrap().waiting.is_some());
        s.arca_handle(Some([7; 16]));
        let Some(Out::SubmitForm { fields, .. }) = s.outbox.pop() else { panic!("{:?}", s.outbox) };
        let got: Vec<_> = fields.iter().map(|f| (f.name.as_str(), f.value.as_str(), f.typed, f.arca)).collect();
        assert_eq!(got, vec![("username", "alice", true, None), ("password", "", true, Some([7; 16])), ("login", "", false, None)]);
        let p = s.form.as_ref().unwrap();
        assert_eq!((p.arca, p.sent), (None, 1), "the handle went with the form; Arca's field is asked anew");
        // No handle in time: the field was empty, the form goes without it.
        s.submit_form(2, None);
        s.arca_tick(std::time::Instant::now() + ARCA_WAIT);
        let Some(Out::SubmitForm { fields, .. }) = s.outbox.pop() else { panic!("{:?}", s.outbox) };
        assert_eq!(fields[1].arca, None);
        assert_eq!(fields[1].value, "");
    }

    #[test]
    fn reloading_a_sign_in_page_discards_the_unconfirmed_form() {
        use limen::doc_viewer::{Field, FieldKind, Form};

        let proposal = |proof: &str| Form {
            action: "/+CSCOE+/saml/sp/continue".into(),
            post: true,
            fields: vec![
                Field { kind: FieldKind::Text, name: "proof".into(), label: "Sign-in code".into(), value: proof.into(), checked: false },
                Field { kind: FieldKind::Submit, name: String::new(), label: "Continue to sign-in".into(), value: String::new(), checked: false },
            ],
        };
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(true);
        s.enter("https://sso.example/+CSCOE+/saml/sp/login");
        s.portal = Some("the corp VPN".into());
        s.set_forms(vec![proposal("old")]);

        s.reload();
        s.submit_form(1, None);
        assert!(s.form.is_none());
        assert_eq!(s.outbox, vec![Out::Reload { key: s.shown_key().unwrap() }]);
        s.outbox.clear();

        s.set_forms(vec![proposal("fresh")]);
        assert!(s.outbox.is_empty());
        s.submit_form(1, None);
        let [Out::SubmitForm { fields, .. }] = s.outbox.as_slice() else { panic!("expected a single confirmed POST") };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].value, "fresh");
    }

    #[test]
    fn the_sign_in_window_sends_its_form_as_filled_on_a_press() {
        use limen::doc_viewer::{Field, FieldKind::*, Form};
        let f = |kind, name: &str, label: &str, value: &str| Field { kind, name: name.into(), label: label.into(), value: value.into(), checked: false };
        let form = Form { action: "/login".into(), post: true, fields: vec![
            f(Fixed, "t", "", "9f"), f(Text, "room", "Room number", ""), f(Password, "pin", "PIN", ""),
            f(Checkbox, "terms", "I accept", "yes"), f(Submit, "go", "Connect", "1"), f(Submit, "", "Cancel", "")] };
        // Not the sign-in window: no panel, whatever the page has.
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(true);
        s.enter("http://10.88.0.1/welcome");
        s.set_forms(vec![form.clone()]);
        assert!(s.form.is_none() && s.geo().form.is_none());
        s.portal = Some("Hotel Guest".into());
        s.set_forms(vec![form.clone()]);
        let p = s.form.as_ref().unwrap();
        assert_eq!((p.rows(), p.buttons(), p.focus), (vec![1, 2, 3], vec![4, 5], Some(1)));
        assert!(s.geo().form.unwrap().h() > em(FORM_HEAD + 3.0 * FORM_ROW));
        for c in "4 12".chars() {
            assert!(s.form_key(Key::Char, Modifiers::default(), &c.to_string()));
        }
        assert!(s.form_key(Key::Tab, Modifiers::default(), ""));
        s.form_key(Key::Char, Modifiers::default(), "7");
        // Enter sends nothing: only a press on a button does.
        s.form_key(Key::Enter, Modifiers::default(), "");
        assert!(s.outbox.is_empty());
        s.set_forms(vec![form.clone()]);
        assert_eq!(s.form.as_ref().unwrap().edits[1].text(), "4 12", "the same form again keeps what was typed");
        s.form_field(3);
        s.submit_form(4, None);
        let Some(Out::SubmitForm { page, action, post, fields, jar, .. }) = s.outbox.pop() else { panic!("{:?}", s.outbox) };
        assert_eq!((page.as_str(), action.as_str(), post), ("http://10.88.0.1/welcome", "/login", true));
        assert!(matches!(jar, navigator_control::Jar::Unrecorded(_)));
        let got: Vec<_> = fields.iter().map(|f| (f.name.as_str(), f.value.as_str(), f.typed)).collect();
        assert_eq!(got, vec![("t", "9f", false), ("room", "4 12", true), ("pin", "7", true), ("terms", "yes", false), ("go", "1", false)]);
        // A press on what isn't a button sends nothing.
        s.submit_form(3, None);
        assert!(s.outbox.is_empty());
        s.set_forms(vec![]);
        assert!(s.form.is_none());
    }

    #[test]
    fn a_page_scripts_values_go_unshown_and_marked_on_a_press() {
        use limen::doc_viewer::{Field, FieldKind::*, Form};
        let f = |kind, name: &str, label: &str, value: &str| Field { kind, name: name.into(), label: label.into(), value: value.into(), checked: false };
        let form = Form { action: "https://idp.example/realms/corp/login-actions/authenticate?x=1".into(), post: true, fields: vec![
            f(Scripted, "clientDataJSON", "", "eyJ0"), f(Scripted, "signature", "", "MEUC"), f(Submit, "", "Continue", "")] };
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(true);
        s.enter("https://idp.example/realms/corp/login-actions/authenticate?x=1");
        s.portal = Some("Corp VPN".into());
        s.set_forms(vec![form]);
        let p = s.form.as_ref().unwrap();
        assert_eq!((p.rows(), p.buttons()), (vec![], vec![2]), "a script's values are never shown");
        assert!(s.outbox.is_empty(), "nothing goes before the press");
        s.submit_form(2, None);
        let Some(Out::SubmitForm { fields, post, .. }) = s.outbox.pop() else { panic!("{:?}", s.outbox) };
        assert!(post);
        let got: Vec<_> = fields.iter().map(|f| (f.name.as_str(), f.value.as_str(), f.typed, f.scripted)).collect();
        assert_eq!(got, vec![("clientDataJSON", "eyJ0", false, true), ("signature", "MEUC", false, true)]);
    }

    #[test]
    fn jump_to_section_lists_the_pages_headings() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(false);
        s.enter("https://atlas.example/wiki/Atrium");
        s.open_find();
        assert!(s.geo().sections_button.is_none(), "no sections, no button");
        s.set_sections(vec![("top".into(), "Atrium".into()), ("Plan".into(), "The plan".into()), ("Rain".into(), "Rain and the impluvium".into())]);
        assert!(s.geo().sections_button.is_some());
        s.toggle_sections();
        assert!(s.sections_open && s.geo().sections_menu.is_some());
        assert_eq!(s.sections_shown().len(), 3);
        // The find bar's text narrows it, unless nothing matches.
        s.find.as_mut().unwrap().edit.set_text("impluv");
        assert_eq!(s.sections_shown(), vec![("Rain".to_string(), "Rain and the impluvium".to_string())]);
        s.find.as_mut().unwrap().edit.set_text("zebra");
        assert_eq!(s.sections_shown().len(), 3);
        s.outbox.clear();
        s.jump_to("nowhere");
        assert!(s.outbox.is_empty() && !s.sections_open, "only the page's own anchors");
        s.toggle_sections();
        s.jump_to("Plan");
        assert_eq!(s.outbox, vec![Out::Jump { anchor: "Plan".into() }]);
        assert!(!s.sections_open);
        // Escape closes the list before the find bar.
        s.toggle_sections();
        assert!(s.escape() && !s.sections_open && s.find.is_some());
    }

    #[test]
    fn a_consent_press_lets_the_site_know_you_and_reopens() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(true);
        s.enter("https://news.example/a");
        s.outbox.clear();
        s.link_pressed("atrium-consent:0");
        let jar = s.store.trail().jar();
        assert!(matches!(jar, navigator_control::Jar::Unrecorded(_)));
        assert_eq!(s.outbox, vec![Out::Activate { page: "https://news.example/a".into(), href: "atrium-consent:0".into(), beside: false, jar }]);
        s.outbox.clear();
        // An answer for another site changes nothing.
        s.consented("https://other.example/");
        assert!(s.outbox.is_empty() && !s.store.knows("news.example"));
        s.consented("https://news.example/a");
        assert!(s.store.knows("news.example"));
        assert!(matches!(s.outbox.as_slice(), [Out::Reload { .. }]), "{:?}", s.outbox);
    }

    #[test]
    fn ctrl_p_prints_only_a_page_shown_and_once_at_a_time() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.store.new_trail(false);
        s.apply(Action::Print, 0.0);
        assert!(s.outbox.is_empty() && !s.printing);
        assert_eq!(s.last_note.as_deref(), Some("nothing to print here"));
        s.live = Some(LivePage::Loading("https://example.org/".into()));
        s.apply(Action::Print, 0.0);
        assert!(s.outbox.is_empty(), "not until it is laid out");
        s.live = Some(LivePage::Shown("https://example.org/".into()));
        s.apply(Action::Print, 0.0);
        assert_eq!(s.outbox, vec![Out::Print]);
        assert!(s.printing);
        s.apply(Action::Print, 0.0);
        assert_eq!(s.outbox.len(), 1, "the sheet is already open");
    }

    #[test]
    fn the_persona_chip_opens_a_window_in_another_persona() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.store.new_trail(false);
        // No other persona: the chip switches new trails, as before.
        s.press_persona_chip();
        assert!(!s.persona_open && s.store.persona == crate::trail::Persona::Unrecorded);
        s.press_persona_chip();
        s.apply(Action::NewWindow, 0.0);
        assert!(!s.persona_open, "Ctrl-N has nothing to offer");
        s.other_personas = vec!["work".into()];
        s.press_persona_chip();
        assert!(s.persona_open && s.geo().persona_menu.is_some());
        let rows: Vec<String> = s.persona_rows().into_iter().map(|r| r.1).collect();
        assert_eq!(rows, ["New trails: Personal", "New trails: Unrecorded", "New Work window"]);
        s.choose_persona_row(1, None);
        assert!(!s.persona_open && s.store.persona == crate::trail::Persona::Unrecorded && s.outbox.is_empty());
        s.apply(Action::NewWindow, 0.0);
        assert!(s.persona_open, "Ctrl-N");
        s.choose_persona_row(2, Some([9; 16]));
        assert_eq!(s.outbox.pop(), Some(Out::NewWindow { persona: "work".into(), press: [9; 16] }));
        assert!(s.persona_open, "up until the answer, so the press is still its window's");
        s.persona_open = false;
        s.press_persona_chip();
        assert!(s.escape() && !s.persona_open);
        // A Work window: its own name, and Personal to open.
        s.window_persona = Some("Work".into());
        s.other_personas = vec!["personal".into()];
        let rows: Vec<String> = s.persona_rows().into_iter().map(|r| r.1).collect();
        assert_eq!(rows, ["New trails: Work", "New trails: Unrecorded", "New Personal window"]);
    }

    #[test]
    fn open_with_sends_the_shown_page_with_its_press() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        assert!(!s.open_with(Some([3; 16])), "no page shown");
        s.live = Some(LivePage::Shown("https://atlas.example/wiki/Atrium".into()));
        assert!(s.open_with(Some([3; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::Hand { url: "https://atlas.example/wiki/Atrium".into(), how: navigator_control::Hand::With, press: [3; 16] }));
    }

    #[test]
    fn the_presence_menu_asks_arca_and_signs_out_here() {
        let mut s = Shell::new(TrailStore::empty(), 1280.0, 696.0);
        s.live = Some(LivePage::Loading(String::new()));
        s.store.new_trail(false);
        s.enter("https://atlas.example/wiki/Atrium");
        let url = "https://atlas.example/wiki/Atrium".to_string();
        s.outbox.clear();
        // A site that doesn't know you: the menu offers remembering only,
        // and Arca isn't asked about accounts.
        s.toggle_presence_menu();
        assert!(s.presence_open && s.geo().presence_menu.is_some());
        assert!(s.outbox.is_empty() && s.accounts.is_none());
        s.presence_choose(true);
        assert!(!s.presence_open && s.store.knows("atlas.example"));
        // Now it does: opening the menu asks which accounts Arca holds.
        s.toggle_presence_menu();
        assert_eq!(s.outbox, vec![Out::Accounts { url: url.clone() }]);
        assert_eq!(s.accounts, Some(("atlas.example".into(), Accounts::Asking)));
        let a = vec![navigator_control::Account { kind: navigator_control::AccountKind::Passkey, name: "alice".into() }];
        s.accounts_answer("https://other.example/", a.clone(), None);
        assert_eq!(s.accounts, Some(("atlas.example".into(), Accounts::Asking)), "an answer for another site is ignored");
        let h0 = presence_height(&s);
        s.accounts_answer(&url, a.clone(), None);
        assert_eq!(s.accounts, Some(("atlas.example".into(), Accounts::Known(a))));
        assert_eq!(presence_height(&s), h0, "one account line, as the asking line");
        // A press outside closes it; Escape too.
        s.press_at(640.0, 600.0);
        assert!(!s.presence_open);
        s.toggle_presence_menu();
        assert!(s.escape() && !s.presence_open);
        // Sign out here: navigatord ends the session cookies, then the page
        // opens again fetched; the site still knows you.
        s.outbox.clear();
        s.toggle_presence_menu();
        s.outbox.clear();
        s.sign_out_here();
        let key = s.shown_key().unwrap();
        assert_eq!(s.outbox, vec![Out::SignOut { jar: navigator_control::Jar::Personal, url: url.clone() }, Out::Reload { key }]);
        assert!(s.store.knows("atlas.example") && !s.presence_open);
        // Forget this site: forgotten, and navigatord told.
        s.outbox.clear();
        s.toggle_presence_menu();
        s.outbox.clear();
        s.presence_choose(false);
        assert_eq!(s.outbox, vec![Out::Forget { jar: navigator_control::Jar::Personal, url }]);
        assert!(!s.store.knows("atlas.example"));
        // Without navigatord, there is no Arca to ask.
        let mut s = shell();
        s.toggle_presence();
        s.toggle_presence_menu();
        assert!(matches!(s.accounts, Some((_, Accounts::Unknown(_)))));
        assert!(s.outbox.is_empty());
    }

    #[test]
    fn typing_and_enter_follow_in_the_trail() {
        let mut s = shell();
        let before = s.store.trail().pages.len();
        s.apply(Action::FocusWhere, 0.0);
        // The address is selected: typing replaces it.
        for c in "roman villa".chars() {
            s.edit_key(Key::Char, Modifiers::default(), &c.to_string(), 0.0);
        }
        assert_eq!(s.editing.as_ref().unwrap().edit.text(), "roman villa");
        s.edit_key(Key::Enter, Modifiers::default(), "", 0.0);
        assert!(s.editing.is_none());
        let t = s.store.trail();
        assert_eq!(t.pages.len(), before + 1);
        assert_eq!(t.title(), "Search: roman villa");
    }

    #[test]
    fn arrow_picks_a_suggestion_and_switches_trail() {
        let mut s = shell();
        s.apply(Action::FocusWhere, 0.0);
        for c in "jails".chars() {
            s.edit_key(Key::Char, Modifiers::default(), &c.to_string(), 0.0);
        }
        s.edit_key(Key::Down, Modifiers::default(), "", 0.0);
        s.edit_key(Key::Enter, Modifiers::default(), "", 0.0);
        assert_eq!(s.store.trail().title(), "Handbook: Jails", "switched to the open trail");
    }

    #[test]
    fn a_link_from_another_app_opens_in_a_trail_of_its_own() {
        let mut s = shell();
        let (n, before) = (s.store.trails.len(), s.shown_url());
        assert!(!s.incoming("mailto:a@b.example", None));
        assert!(!s.incoming("javascript:alert(1)", None));
        assert_eq!(s.store.trails.len(), n);
        assert!(s.incoming("https://atlas.example/wiki/Atrium", None));
        assert_eq!(s.store.trails.len(), n + 1);
        assert_eq!(s.shown_url().as_deref(), Some("https://atlas.example/wiki/Atrium"));
        assert!(s.editing.is_none());
        assert_ne!(s.shown_url(), before);
    }

    /// personas.md §6: a link the person sent into Work from Personal opens
    /// in a new trail where no site knows you, though Work's jar knows it,
    /// until the person lets the site know them there.
    #[test]
    fn a_link_sent_into_work_opens_anonymous() {
        let mut s = shell();
        s.store.set_knows("atlas.example", true);
        // In the Personal window: as any link.
        assert!(s.incoming("https://atlas.example/wiki/Atrium", Some("work")));
        assert!(!s.store.trail().anonymous && s.store.knows("atlas.example"));
        s.window_persona = Some("work".into());
        assert!(s.incoming("https://atlas.example/wiki/Impluvium", None));
        assert!(!s.store.trail().anonymous, "from Work itself");
        assert!(s.incoming("https://atlas.example/wiki/Impluvium", Some("personal")));
        let t = s.store.trail().id;
        assert!(s.store.trail().anonymous && !s.store.knows("atlas.example"));
        assert_eq!(s.store.presence_of(t, "https://atlas.example/wiki/Impluvium").map(|p| p.1), Some(false));
        assert!(s.last_note.as_deref().unwrap().contains("sent from Personal"));
        s.store.set_knows("atlas.example", true);
        assert!(!s.store.trail().anonymous && s.store.knows("atlas.example"), "the person let it");
    }

    #[test]
    fn new_trail_focuses_where_and_escape_leaves_start() {
        let mut s = shell();
        s.apply(Action::NewTrail, 0.0);
        assert!(s.store.trail().current.is_none());
        assert!(s.editing.is_some());
        s.edit_key(Key::Escape, Modifiers::default(), "", 0.0);
        assert!(s.editing.is_none());
    }

    #[test]
    fn links_follow_beside_and_download() {
        let mut s = shell();
        let (_, links) = fake::lookup(&s.store.trail().page().unwrap().url);
        let cur = s.store.trail().current;
        s.mods.ctrl = true;
        s.activate(&links[0], 0.0);
        assert_eq!(s.store.trail().current, cur, "Ctrl-click opens beside");
        s.mods.ctrl = false;
        let pdf = links.iter().find(|l| l.download.is_some()).unwrap();
        s.activate(pdf, 1000.0);
        assert_eq!(s.store.trail().current, cur, "a file asks, it doesn't navigate");
        assert!(s.geo().prompt.is_some());
        assert!(!s.answer(1, 1200.0), "too soon after it appeared");
        assert!(s.answer(1, 1600.0));
        assert!(s.geo().prompt.is_none());
        s.activate(&links[0], 2000.0);
        assert_ne!(s.store.trail().current, cur);
    }

    #[test]
    fn find_asks_the_page_and_shows_its_count() {
        let mut s = shell();
        let none = Modifiers::default();
        s.apply(Action::Find, 0.0);
        assert!(s.find.is_some() && s.geo().find.is_some());
        assert!(s.geo().page.y() > bar() + FIND, "the bar pushes the page down");
        for c in ["a", "t"] {
            s.find_key(Key::Char, none, c);
        }
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: "at".into(), step: 0 }));
        assert!(!s.find.as_ref().unwrap().answered);
        s.found(3, 1);
        s.find_key(Key::Enter, none, "");
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: "at".into(), step: 1 }));
        s.find_key(Key::Enter, Modifiers { shift: true, ..none }, "");
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: "at".into(), step: -1 }));
        // Another page shown: find closes with it.
        s.store.trail_mut().back();
        s.settle(0.0);
        assert!(s.find.is_none());
        s.apply(Action::Find, 0.0);
        assert!(s.escape());
        assert!(s.find.is_none());
        assert_eq!(s.outbox.last(), Some(&Out::Find { query: String::new(), step: 0 }), "Esc clears the page's mark");
    }

    #[test]
    fn reading_choices_are_per_site_and_zoom_keys_step() {
        let mut s = shell();
        let url = s.shown_url().unwrap();
        s.reading_change(|r| r.text_64 = crate::trail::ReadingPrefs::step(r.text_64, true));
        assert_eq!(s.store.reading().text_64, 72);
        s.apply(Action::ZoomIn, 0.0);
        s.apply(Action::ZoomIn, 0.0);
        assert_eq!(s.store.reading().zoom_64, 80, "110% then 125%");
        s.apply(Action::ZoomReset, 0.0);
        assert_eq!(s.store.reading().zoom_64, 64);
        let t = s.store.trail().id;
        assert_eq!(s.store.reading_of(t, &url).text_64, 72);
        assert_eq!(s.store.reading_of(t, "https://elsewhere.example/").text_64, 64, "another site keeps its own");
        s.reading_open = true;
        assert!(s.geo().reading.is_some());
        s.apply(Action::Back, 0.0);
        assert!(!s.reading_open, "a key closes the pop-up");
    }

    #[test]
    fn history_filters_moves_and_opens() {
        let mut s = shell();
        s.apply(Action::History, 0.0);
        assert!(s.history.is_some() && s.geo().prompt.is_none());
        let all = s.history_rows().len();
        assert!(all > 1);
        let pick = s.history_rows()[1].clone();
        s.history_key(Key::Down, Modifiers::default(), "");
        s.history_key(Key::Enter, Modifiers::default(), "");
        assert!(s.history.is_none());
        assert_eq!(s.shown_url().as_deref(), Some(pick.url.as_str()));
        s.apply(Action::History, 0.0);
        for c in "zzzz".chars() {
            s.history_key(Key::Char, Modifiers::default(), &c.to_string());
        }
        assert!(s.history_rows().is_empty());
        s.apply(Action::History, 0.0);
        assert!(s.history.is_none(), "Ctrl-H again closes it");
    }

    #[test]
    fn a_page_from_history_offers_its_current_version_and_history_clears_after_asking() {
        let mut s = shell();
        // Give every page a stored version, as navigatord would.
        for t in s.store.trails.iter_mut().chain(s.store.earlier.iter_mut().map(|e| &mut e.trail)) {
            for (i, p) in t.pages.iter_mut().enumerate() {
                p.saved = Some(format!("{:0>60}{:04}", t.id % 1000, i));
            }
        }
        s.apply(Action::History, 0.0);
        let pick = s.history_rows()[1].clone();
        s.open_history_row(&pick, 10_000.0);
        let asked = s.store.trail().page().unwrap().asked.clone().unwrap();
        assert!(matches!(asked.prompt, Prompt::Stored { .. }), "the stored version, and an offer");
        assert!(s.answer(1, 10_000.0 + PROMPT_STILL_MS));
        let key = s.shown_key().unwrap();
        assert_eq!(s.outbox.pop(), Some(Out::Reload { key }));
        assert!(s.store.trail().page().unwrap().saved.is_none(), "it no longer opens as the stored version");
        assert!(s.store.trail().page().unwrap().asked.is_none());
        // Ctrl-R does the same.
        s.apply(Action::Reload, 0.0);
        assert_eq!(s.outbox.pop(), Some(Out::Reload { key }));

        // Clearing asks first, and Esc takes the question back, not the view.
        s.apply(Action::History, 0.0);
        let before = s.history_rows().len();
        s.ask_clear(crate::history::Clear::All);
        let n = s.history.as_ref().unwrap().confirm.as_ref().unwrap().1;
        assert!(n > 0 && n < before + 10);
        assert!(s.escape());
        assert!(s.history.as_ref().is_some_and(|h| h.confirm.is_none()));
        assert_eq!(s.history_rows().len(), before, "nothing went");
        s.ask_clear(crate::history::Clear::All);
        s.history_key(Key::Enter, Modifiers::default(), "");
        let Some(Out::Clear { keys, records }) = s.outbox.pop() else { panic!("no Clear") };
        assert!(!keys.is_empty() && !records.is_empty());
        assert!(!records.iter().any(|r| s.store.trails.iter().any(|t| t.page().and_then(|p| p.saved.as_ref()) == Some(r))), "no open trail's page's version");
        let open = s.store.trails.iter().filter(|t| !t.unrecorded && t.current.is_some()).count();
        assert_eq!(s.history_rows().len(), open, "each open trail keeps its page");
        assert!(s.store.earlier.is_empty());
    }

    #[test]
    fn f6_goes_round_rail_toolbar_and_page() {
        let mut s = shell();
        assert!(s.store.trail().page().is_some());
        s.apply(Action::NextRegion, 0.0);
        assert!(s.rail_focus && s.editing.is_none(), "from nothing focused: the rail");
        let before = s.store.current;
        assert!(s.rail_key(Key::Down));
        assert!(s.rail_focus, "Down moves through the trails and keeps the rail");
        assert_ne!(s.store.current, before);
        s.rail_key(Key::Up);
        assert_eq!(s.store.current, before);
        s.apply(Action::NextRegion, 0.0);
        assert!(!s.rail_focus && s.editing.is_some(), "then the toolbar's Where field");
        s.apply(Action::NextRegion, 0.0);
        assert!(s.editing.is_none() && s.focus_page, "then the page");
        s.focus_page = false;
        s.region_from_page(true);
        assert!(s.rail_focus, "F6 handed back by the page: the rail");
        s.apply(Action::PrevRegion, 0.0);
        assert!(s.focus_page && !s.rail_focus, "Shift-F6 from the rail: the page");
        s.focus_page = false;
        // No page to give the keys to: the history view, or Start.
        s.apply(Action::History, 0.0);
        s.region_from_page(false);
        assert!(s.editing.is_some());
        s.apply(Action::NextRegion, 0.0);
        assert!(s.rail_focus && !s.focus_page, "the page is skipped while history is up");
        assert!(s.escape() && !s.rail_focus);
    }

    #[test]
    fn widget_row_uses_only_the_current_documents_recorded_origin() {
        let mut s = shell();
        s.enter("https://parent.example/one");
        let key = s.shown_key().unwrap();
        let sources = vec![("/counter".into(), "https://parent.example".into())];
        assert!(!s.widget_pressed(key, &sources, "https://other.example", 17, [1., 2., 30., 40.], false, 0.0));
        assert!(s.widget_pressed(key, &sources, "/counter", 17, [1., 2., 30., 40.], false, 0.0));
        let prompt = &s.store.trail().page().unwrap().asked.as_ref().unwrap().prompt;
        assert_eq!(crate::ax::prompt_text(prompt), "Live widget from https://parent.example is unavailable");
        assert_eq!(prompt.answers(), &["Close"]);
        assert!(s.answer_pressed(0, 700.0, Some([3; 16])));
        assert!(s.outbox.is_empty());
        s.enter("https://parent.example/two");
        assert!(!s.widget_pressed(key, &sources, "/counter", 17, [1., 2., 30., 40.], false, 1500.0));
    }

    #[test]
    fn replacing_the_selected_source_with_the_same_key_retires_the_old_question() {
        let mut s = shell();
        s.enter("https://parent.example/");
        let page = s.shown_key().unwrap();
        let sources = vec![("/one".into(), "https://parent.example".into()),
                           ("/two".into(), "https://parent.example".into())];
        let rect = [2., 3., 100., 120.];
        assert!(s.widget_pressed(page, &sources, "/one", 18, rect, false, 0.));
        assert!(s.widget_pressed(page, &sources, "/two", 18, rect, false, 100.));
        assert!(s.store.trail().page().unwrap().asked.is_none());
        assert!(s.widget_pressed(page, &sources, "/two", 18, rect, false, 1500.));
        let asked = s.store.trail().page().unwrap().asked.as_ref().unwrap();
        assert!(matches!(&asked.prompt, Prompt::Widget { src, .. } if src == "/two"));
        assert_eq!(asked.since_ms, 1500.);
        assert!(s.widget_pressed(page, &sources, "/unrecorded", 18, rect, false, 1600.));
        assert!(s.store.trail().page().unwrap().asked.is_none());
    }

    #[test]
    fn withdrawing_a_widget_preserves_unrelated_questions_and_their_guard() {
        let mut s = shell();
        s.enter("https://parent.example/one");
        let page = s.shown_key().unwrap();
        let sources = vec![("/counter".into(), "https://parent.example".into())];
        let rect = [2., 3., 100., 120.];
        assert!(s.widget_pressed(page, &sources, "/counter", 18, rect, false, 0.));
        assert!(s.ask(Prompt::RunLive, 1200.));
        assert!(!s.widget_placement(page, Some(11), None, 1400.));
        let moved = [3., 0., 100., 90.];
        assert!(s.widget_placement(page, Some(18), Some(moved), 1400.));
        assert!(matches!(s.store.trail().page().unwrap().asked.as_ref().unwrap().prompt,
            Prompt::Widget { rect, .. } if rect == moved));
        assert!(s.widget_placement(page, None, None, 1500.));
        let next = s.store.trail().page().unwrap().asked.as_ref().unwrap();
        assert_eq!(next.prompt, Prompt::RunLive);
        assert_eq!(next.since_ms, 1500.);
        assert!(s.widget_pressed(page, &sources, "/counter", 18, rect, false, 2500.));
        assert!(s.widget_placement(page, Some(18), None, 2600.));
        assert!(s.store.trail().page().unwrap().prompts.queued.is_empty());
        assert!(!s.widget_placement(page, Some(18), Some(moved), 2700.));
    }

    #[test]
    fn accepting_the_live_offer_passes_the_current_address_jar_and_press() {
        let mut s = shell();
        let url = navigator_control::INBOX_FIXTURE_URL;
        s.enter(url);
        s.live = Some(LivePage::LiveOffer);
        assert!(s.ask(Prompt::RunLive, 0.0));
        assert!(!s.answer_pressed(1, 100.0, Some([3; 16])));
        assert!(s.outbox.is_empty());
        assert!(s.answer_pressed(1, 700.0, Some([3; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::RunLive {
            url: url.into(), jar: s.store.trail().jar(), press: [3; 16],
        }));
        s.press_refused("covered", 1000.0);
        assert!(s.answer_pressed(0, 1700.0, Some([4; 16])));
        assert!(s.outbox.is_empty());
    }

    #[test]
    fn a_page_runs_live_only_on_a_counted_press_and_stop_brings_the_document_back() {
        let mut s = shell();
        let url = "https://app.example/app.html";
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        // Off unless navigatord turned it on: no offer, no prompt.
        assert_eq!(popover_action(&s), None);
        assert!(!s.offer_live(0.0));
        s.live_pages = true;
        assert_eq!(popover_action(&s), Some(PopoverAction::RunLive));
        // The popover's row only asks; nothing is sent yet.
        s.popover = true;
        assert!(s.offer_live(0.0));
        assert!(!s.popover);
        assert!(s.outbox.is_empty());
        let asked = s.store.trail().page().unwrap().asked.clone().unwrap();
        assert_eq!(asked.prompt, Prompt::RunPageLive { site: "app.example".into(), recorded: true });
        // "Not now", then asked again from the popover: the person's choice.
        assert!(s.answer_pressed(0, 600.0, Some([1; 16])));
        assert!(s.outbox.is_empty() && s.live_run.is_none());
        assert!(s.offer_live(700.0));
        // A press before the row was still for 500 ms does nothing.
        assert!(!s.answer_pressed(2, 900.0, Some([2; 16])));
        assert!(s.outbox.is_empty());
        assert!(s.answer_pressed(2, 1300.0, Some([2; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::RunPageLive { url: url.into(), press: [2; 16], jar: navigator_control::Jar::Personal, remember: false }));
        assert_eq!(popover_action(&s), Some(PopoverAction::Stop));
        // The chip reads Live code only once the engine started the page.
        assert_ne!(s.store.trail().page().unwrap().display_facts().tier, crate::trail::Tier::Live);
        s.live_reported(url, Some("https://app.example".into()), None);
        assert_eq!(s.store.trail().page().unwrap().display_facts().tier, crate::trail::Tier::Live);
        s.stop_live();
        assert_eq!(s.outbox.pop(), Some(Out::StopLive { url: url.into() }));
        assert_ne!(s.store.trail().page().unwrap().display_facts().tier, crate::trail::Tier::Live);
        assert_eq!(popover_action(&s), Some(PopoverAction::RunLive));
        // An engine that ends by itself does the same, with why.
        assert!(s.offer_live(2000.0));
        assert!(s.answer_pressed(2, 2600.0, Some([5; 16])));
        s.live_reported(url, Some("https://app.example".into()), None);
        s.live_reported(url, None, Some("the page's engine failed".into()));
        assert!(s.live_run.is_none());
        assert_ne!(s.store.trail().page().unwrap().display_facts().tier, crate::trail::Tier::Live);
        assert!(s.last_note.as_deref().is_some_and(|n| n.contains("engine failed")));
    }

    #[test]
    fn a_live_pages_file_is_asked_on_its_entry_and_its_refusals_are_named_in_the_popover() {
        let mut s = shell();
        let url = "https://app.example/app.html";
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        s.live_pages = true;
        assert!(s.offer_live(0.0));
        assert!(s.answer_pressed(2, 600.0, Some([2; 16])));
        s.outbox.clear();
        let key = s.shown_key().unwrap();
        let dl = |id: u32| Prompt::Download { id, file: "report.csv".into(), size: "12 B".into(), origin: "https://app.example".into(), kind: "CSV file".into() };
        assert!(!s.live_file(url, dl(1 << 31), 10_000.0), "not before the engine started");
        s.live_reported(url, Some("https://app.example".into()), None);
        assert!(!s.live_file("https://app.example/other.html", dl(1 << 31), 10_000.0), "only the page that runs live");
        assert!(s.live_file(url, dl(1 << 31), 10_000.0));
        let asked = s.store.page_by_key_mut(key.0, key.1).and_then(|p| p.asked.as_ref()).map(|a| a.prompt.clone());
        assert_eq!(asked, Some(dl(1 << 31)), "on the live page's own entry");
        assert!(s.live_run.is_some(), "the page keeps running");
        // Save on it sends the held file's id.
        assert!(s.answer_pressed(2, 20_000.0, Some([3; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::Save { id: 1 << 31, press: [3; 16] }));
        // window.print(): the entry's rendering prints, once at a time; not
        // for a page that isn't the one running.
        assert!(s.live_on_screen(url) && !s.live_on_screen("https://app.example/other.html"));
        assert!(!s.live_print("https://app.example/other.html"));
        assert!(s.live_print(url));
        assert_eq!(s.outbox.pop(), Some(Out::Print));
        assert!(!s.live_print(url), "one print at a time");
        // A paste too long for the page is said in the row, with only Close.
        assert!(s.live_file(url, Prompt::PasteTooLong, 30_000.0));
        let asked = s.store.page_by_key_mut(key.0, key.1).and_then(|p| p.asked.as_ref()).map(|a| a.prompt.clone());
        assert_eq!(asked, Some(Prompt::PasteTooLong));
        assert_eq!(Prompt::PasteTooLong.answers(), &["Close"]);
        // Refusals: each once, bounded, and in the popover while it is shown.
        assert_eq!(s.live_refusals(), None);
        assert!(s.live_refused(url, "notifications"));
        assert!(s.live_refused(url, "notifications"));
        assert!(s.live_refused(url, "a request to cdn.thirdparty.example"));
        assert!(!s.live_refused(url, "a\nb"), "no control characters");
        assert!(!s.live_refused("https://other.example/", "the camera"), "another page's");
        for i in 0..40 { s.live_refused(url, &format!("api {i}")); }
        assert_eq!(s.live_run.as_ref().unwrap().refused.len(), LIVE_REFUSED_MAX);
        assert!(s.live_refusals().unwrap().starts_with("Refused here: notifications; a request to cdn.thirdparty.example; api 0"));
        s.popover = true;
        let with_refusals = popover_height(&s, 360.0);
        s.live_run.as_mut().unwrap().refused.clear();
        assert!(popover_height(&s, 360.0) < with_refusals, "the popover makes room for them");
    }

    #[test]
    fn a_live_page_link_is_a_trail_entry_that_runs_live_at_the_same_origin() {
        let mut s = shell();
        let url = "https://app.example/app.html";
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        s.live_pages = true;
        assert!(s.offer_live(0.0));
        assert!(s.answer_pressed(2, 600.0, Some([2; 16])));
        s.outbox.clear();
        let first = s.shown_key().unwrap();
        // Before the engine started, a proposal is not followed.
        assert!(!s.live_navigated(url, "https://app.example/second.html"));
        s.live_reported(url, Some("https://app.example".into()), None);
        // Only the page that runs live can propose.
        assert!(!s.live_navigated("https://app.example/other.html", "https://app.example/second.html"));
        assert!(s.live_navigated(url, "https://app.example/second.html"));
        let second = s.shown_key().unwrap();
        assert_ne!(first, second);
        assert_eq!(s.shown_url().as_deref(), Some("https://app.example/second.html"));
        // While navigatord opens it, nothing is sent.
        s.live = Some(LivePage::Loading("https://app.example/second.html".into()));
        assert!(!s.live_follow());
        assert!(s.outbox.is_empty());
        // Shown (after a redirect within the origin): it runs live there.
        s.live = Some(LivePage::Shown("https://app.example/second.html?r=1".into()));
        assert!(s.live_follow());
        assert_eq!(s.outbox.pop(), Some(Out::ContinueLive { url: "https://app.example/second.html?r=1".into(), jar: navigator_control::Jar::Personal }));
        let run = s.live_run.clone().unwrap();
        assert_eq!((run.key, run.entries), (second, vec![first, second]));
        s.live_reported("https://app.example/second.html?r=1", Some("https://app.example".into()), None);
        assert_eq!(s.store.trail().page().unwrap().display_facts().tier, crate::trail::Tier::Live);
        // A page of another origin opens in the document lane: live stops.
        assert!(s.live_navigated("https://app.example/second.html?r=1", "https://other.example/"));
        s.live = Some(LivePage::Shown("https://other.example/".into()));
        assert!(s.live_follow());
        assert_eq!(s.outbox.pop(), Some(Out::StopLive { url: "https://app.example/second.html?r=1".into() }));
        assert!(s.live_run.is_none());
        // The person goes elsewhere before the proposed page is shown: stop.
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        assert!(s.offer_live(5000.0));
        assert!(s.answer_pressed(2, 5600.0, Some([3; 16])));
        s.live_reported(url, Some("https://app.example".into()), None);
        s.outbox.clear();
        assert!(s.live_navigated(url, "https://app.example/third.html"));
        s.enter("https://app.example/typed.html");
        s.live = Some(LivePage::Shown("https://app.example/typed.html".into()));
        assert!(s.live_follow());
        assert_eq!(s.outbox.pop(), Some(Out::StopLive { url: url.into() }));
    }

    #[test]
    fn a_live_pages_new_window_is_an_entry_beside_it_and_the_page_runs_on() {
        let mut s = shell();
        let url = "https://app.example/app.html";
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        s.live_pages = true;
        assert!(s.offer_live(0.0));
        assert!(s.answer_pressed(2, 600.0, Some([2; 16])));
        s.outbox.clear();
        let here = s.shown_key().unwrap();
        assert!(!s.live_opened_beside(url, "https://elsewhere.example/"), "not before the engine started");
        s.live_reported(url, Some("https://app.example".into()), None);
        assert!(!s.live_opened_beside("https://app.example/other.html", "https://elsewhere.example/"), "only the page that runs live");
        assert!(s.live_opened_beside(url, "https://elsewhere.example/"));
        assert_eq!(s.shown_key(), Some(here), "the page stays on screen");
        assert!(s.live_run.is_some(), "and keeps running");
        assert!(s.outbox.is_empty());
        let t = s.store.trail();
        let beside: Vec<_> = t.pages[here.1].children.iter().map(|&c| t.pages[c].url.clone()).collect();
        assert_eq!(beside, ["https://elsewhere.example/"]);
    }

    #[test]
    fn a_live_page_route_is_a_trail_entry_in_the_same_engine() {
        let mut s = shell();
        let url = "https://app.example/app.html";
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        s.live_pages = true;
        assert!(s.offer_live(0.0));
        assert!(s.answer_pressed(2, 600.0, Some([2; 16])));
        let home = s.shown_key().unwrap();
        // Not before the engine started, not from another page, not another origin.
        assert!(!s.live_url_changed(url, "https://app.example/route/1"));
        s.live_reported(url, Some("https://app.example".into()), None);
        s.outbox.clear();
        assert!(!s.live_url_changed("https://app.example/other.html", "https://app.example/route/1"));
        assert!(!s.live_url_changed(url, "https://api.app.example/route/1"));
        assert!(!s.live_url_changed(url, url), "the same address is no entry");
        // pushState: a new entry, the engine is on it, nothing is sent.
        assert!(s.live_url_changed(url, "https://app.example/route/1"));
        let route = s.shown_key().unwrap();
        assert_ne!(route, home);
        assert_eq!(s.shown_url().as_deref(), Some("https://app.example/route/1"));
        assert!(s.live_route(route) && !s.live_route(home));
        assert_eq!(s.store.trail().page().unwrap().display_facts().tier, crate::trail::Tier::Live);
        assert!(!s.live_follow());
        assert!(s.outbox.is_empty());
        // The page goes back to its first address itself: the entry before.
        assert!(s.live_url_changed(url, url));
        assert_eq!(s.shown_key(), Some(home));
        assert!(s.live_url_changed(url, "https://app.example/route/1"));
        assert_eq!(s.shown_key(), Some(route), "forward to the entry it made, not a new one");
        // The person's Back: the first entry runs live again at its own address.
        s.store.trail_mut().back();
        s.live = Some(LivePage::Shown(url.into()));
        assert!(s.live_follow());
        assert_eq!(s.outbox.pop(), Some(Out::ContinueLive { url: url.into(), jar: navigator_control::Jar::Personal }));
        assert_eq!(s.live_run.as_ref().unwrap().key, home);
        // Forward to the route: it is opened like a page, then runs live.
        s.live_reported(url, Some("https://app.example".into()), None);
        s.store.trail_mut().forward();
        s.live = Some(LivePage::Loading("https://app.example/route/1".into()));
        assert!(!s.live_follow());
        s.live = Some(LivePage::Shown("https://app.example/route/1".into()));
        assert!(s.live_follow());
        assert_eq!(s.outbox.pop(), Some(Out::ContinueLive { url: "https://app.example/route/1".into(), jar: navigator_control::Jar::Personal }));
    }

    #[test]
    fn a_site_chosen_always_runs_live_unasked_until_the_chip_asks_again() {
        let mut s = shell();
        let url = "https://app.example/app.html";
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        s.live_pages = true;
        assert!(s.offer_live(0.0));
        assert_eq!(s.store.trail().page().unwrap().asked.as_ref().unwrap().prompt.answers(), &["Not now", "Always for this site", "Run live"]);
        assert!(s.answer_pressed(1, 600.0, Some([2; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::RunPageLive { url: url.into(), press: [2; 16], jar: navigator_control::Jar::Personal, remember: true }));
        // navigatord remembered it; the page runs already, so nothing more.
        assert!(!s.live_remembered(url));
        assert!(s.outbox.is_empty());
        s.live_reported(url, Some("https://app.example".into()), None);
        assert_eq!(popover_actions(&s), vec![PopoverAction::Stop, PopoverAction::Forget]);
        s.stop_live();
        s.outbox.clear();
        // Another page of the site, opened later: it runs live on navigatord's word.
        let next = "https://app.example/next.html";
        s.enter(next);
        s.live = Some(LivePage::Loading(next.into()));
        assert!(!s.live_remembered(next), "not before it is shown");
        s.live = Some(LivePage::Shown(next.into()));
        assert!(!s.live_remembered("https://app.example/other.html"), "not for a page not on screen");
        assert!(s.live_remembered(next));
        assert_eq!(s.outbox.pop(), Some(Out::ContinueLive { url: next.into(), jar: navigator_control::Jar::Personal }));
        assert!(s.live_run.as_ref().is_some_and(|r| r.url == next));
        // The chip's row: ask again. What runs goes on.
        s.forget_live();
        assert_eq!(s.outbox.pop(), Some(Out::ForgetLive { url: next.into() }));
        assert!(s.live_run.is_some());
        assert_eq!(popover_actions(&s), vec![PopoverAction::Stop]);
        // An Unrecorded trail only asks, and never runs unasked.
        s.stop_live();
        s.outbox.clear();
        s.new_trail(true);
        s.enter(url);
        s.live = Some(LivePage::Shown(url.into()));
        assert!(s.offer_live(0.0));
        assert_eq!(s.store.trail().page().unwrap().asked.as_ref().unwrap().prompt.answers(), &["Not now", "Run live"]);
        s.store.trail_mut().page_mut().unwrap().asked = None;
        assert!(!s.live_remembered(url));
        assert!(s.outbox.is_empty() && s.live_run.is_none());
        assert!(!popover_actions(&s).contains(&PopoverAction::Forget));
    }

    #[test]
    fn an_http_page_is_not_offered_live() {
        let mut s = shell();
        s.enter("http://plain.example/");
        s.live = Some(LivePage::Shown("http://plain.example/".into()));
        s.live_pages = true;
        assert_eq!(popover_action(&s), None);
    }

    #[test]
    fn a_press_frescod_did_not_count_brings_the_question_back() {
        let mut s = shell();
        s.live = Some(LivePage::Shown(String::new()));
        let dl = |id: u32, f: &str| Prompt::Download { id, file: f.into(), size: "1 KB".into(), origin: "atlas.example".into(), kind: "PDF".into() };
        assert!(s.ask(dl(7, "a.pdf"), 0.0));
        assert!(s.answer_pressed(2, 600.0, Some([5; 16])), "Save");
        assert_eq!(s.outbox.pop(), Some(Out::Save { id: 7, press: [5; 16] }), "the press goes with it");
        assert!(s.store.trail().page().unwrap().asked.is_none());
        // Something else asked meanwhile: the refused one goes in front.
        assert!(s.ask(dl(8, "b.pdf"), 1700.0));
        s.press_refused("something was over the question", 1800.0);
        let page = s.store.trail().page().unwrap();
        assert!(matches!(&page.asked, Some(a) if a.prompt.key().contains("a.pdf") && a.since_ms == 1800.0));
        assert!(page.prompts.queued.iter().any(|p| p.key().contains("b.pdf")));
        assert!(!s.answer(2, 2000.0), "its own 500 ms again");
        assert!(s.answer_pressed(2, 2400.0, Some([6; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::Save { id: 7, press: [6; 16] }));
        // Nothing sent with a press: nothing to put back.
        s.answered = None;
        s.press_refused("x", 3000.0);
        assert!(s.last_note.as_deref().is_some_and(|n| n.contains("didn't count")));
    }

    #[test]
    fn the_prompt_row_waits_takes_turns_and_remembers_no() {
        let mut s = shell();
        s.live = Some(LivePage::Shown(String::new()));
        let dl = |id: u32, f: &str| Prompt::Download { id, file: f.into(), size: "1 KB".into(), origin: "atlas.example".into(), kind: "PDF".into() };
        assert!(s.ask(dl(7, "a.pdf"), 0.0));
        assert!(!s.ask(dl(8, "b.pdf"), 500.0), "one a second at most");
        assert!(s.ask(dl(8, "b.pdf"), 1000.0), "queued behind the first");
        assert!(!s.ask(dl(8, "b.pdf"), 2000.0), "the same one twice");
        // Covered by the tier popover: a press doesn't count, and the row's
        // 500 ms start again once it is uncovered.
        s.settle(0.0);
        s.popover = true;
        assert!(s.prompt_obscured());
        assert!(!s.answer(0, 2000.0));
        s.settle(2000.0);
        s.popover = false;
        assert!(!s.answer(0, 2200.0), "500 ms from when it was last covered");
        assert!(s.answer(0, 2600.0), "Cancel");
        assert_eq!(s.outbox.pop(), Some(Out::Discard { id: 7 }));
        // The next one takes its own 500 ms.
        assert!(matches!(&s.store.trail().page().unwrap().asked, Some(a) if a.prompt.key().contains("b.pdf")));
        assert!(!s.answer(2, 2700.0));
        assert!(s.answer(2, 3200.0), "Save");
        assert_eq!(s.outbox.pop(), Some(Out::Save { id: 8, press: [0; 16] }));
        // Open: the held file goes to the person's app for it, with the press.
        assert!(s.ask(dl(11, "d.pdf"), 4300.0));
        assert!(s.answer_pressed(1, 4900.0, Some([7; 16])), "Open");
        assert_eq!(s.outbox.pop(), Some(Out::OpenFile { id: 11, press: [7; 16] }));
        assert!(s.geo().prompt.is_none());
        assert!(!s.ask(dl(9, "a.pdf"), 9000.0), "dismissed on this page: not asked again");
        // A row that moves starts again too.
        assert!(s.ask(dl(10, "c.pdf"), 9000.0));
        s.settle(9000.0);
        s.rail_collapsed = !s.rail_collapsed;
        assert!(s.settle(9600.0), "moved");
        assert!(!s.answer(1, 9700.0));
        assert!(s.answer(1, 10100.0));
    }

    #[test]
    fn hand_offs_ask_and_name_the_app() {
        let mut s = shell();
        s.live = Some(LivePage::Shown(String::new()));
        s.handoff("atrium-app://example.com/photos?path=/x", "atrium-app", None, 0.0);
        let a = s.store.trail().page().unwrap().asked.clone().unwrap();
        assert_eq!(a.prompt, Prompt::OpenApp { url: "atrium-app://example.com/photos?path=/x".into(), name: "example.com".into(), about: None });
        assert_eq!(a.prompt.answers(), &["Not now", "Open"]);
        assert!(!s.answer(2, 600.0));
        assert!(s.answer_pressed(1, 600.0, Some([1; 16])), "Open");
        assert_eq!(s.outbox.pop(), Some(Out::Hand { url: "atrium-app://example.com/photos?path=/x".into(), how: navigator_control::Hand::Open, press: [1; 16] }));
        s.handoff("mailto:a@atlas.example", "mailto", None, 2000.0);
        assert_eq!(s.store.trail().page().unwrap().asked.as_ref().unwrap().prompt.answers(), &["Not now", "Open"]);
        assert!(s.answer(1, 2600.0));
        assert_eq!(s.outbox.pop(), Some(Out::Hand { url: "mailto:a@atlas.example".into(), how: navigator_control::Hand::Open, press: [0; 16] }));
    }

    #[test]
    fn the_app_prompt_says_who_publishes_it_and_what_it_asks_for() {
        use navigator_control::AppAbout;
        let about = |caps: &[&str], installed| AppAbout { name: "Photos".into(), publisher: "Example Ltd (example.com)".into(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(), installed, third_party: true };
        let p = Prompt::OpenApp { url: "atrium-app://org.example.photos".into(), name: "org.example.photos".into(), about: Some(about(&["files", "network-read"], true)) };
        assert_eq!(crate::ax::prompt_text(&p), "This link opens the app Photos by Example Ltd (example.com); it asks for files, network-read");
        let w = crate::trail::app_words("x", Some(&about(&["a", "b", "c", "d", "e"], false)));
        assert_eq!(w.2, "asks for a, b, c and 2 more; not installed, Open asks to install it");
        let p = Prompt::OpenApp { url: "atrium-app://org.example.side".into(), name: "org.example.side".into(), about: None };
        assert_eq!(crate::ax::prompt_text(&p), "This link opens the app org.example.side, not in the package catalog");
    }

    #[test]
    fn stop_page_goes_back() {
        let mut s = Shell::scene(Scene::Stop, 1280.0, 696.0);
        s.leave_stopped();
        assert_eq!(s.store.trail().title(), "Inbox (3)");
    }

    #[test]
    fn geometry() {
        let mut s = shell();
        let g = s.geo();
        assert_eq!(g.rail, RAIL);
        assert_eq!(g.page.y(), bar() + 1.0);
        assert_eq!(g.page.y() + g.page.h(), 696.0);
        s.apply(Action::ToggleStatus, 0.0);
        s.apply(Action::ToggleRail, 0.0);
        let g = s.geo();
        assert_eq!(g.rail, RAIL_NARROW);
        assert_eq!(g.page.y() + g.page.h(), 696.0 - STATUS);
        s.w = 800.0;
        s.rail_collapsed = false;
        assert_eq!(s.geo().rail, RAIL_NARROW, "narrow windows collapse the rail");
    }

    #[test]
    fn press_outside_closes_the_field() {
        let mut s = shell();
        s.apply(Action::FocusWhere, 0.0);
        let g = s.geo();
        s.press_at(g.where_.x() + 5.0, g.where_.y() + 5.0);
        assert!(s.editing.is_some());
        s.press_at(10.0, 600.0);
        assert!(s.editing.is_none());
    }

    #[test]
    fn put_away_and_bring_back() {
        let mut s = shell();
        let title = s.store.trail().title().to_string();
        s.apply(Action::PutAway, 0.0);
        assert_ne!(s.store.trail().title(), title);
        s.apply(Action::BringBack, 0.0);
        assert_eq!(s.store.trail().title(), title);
    }

    #[test]
    fn the_toolbar_holds_its_tools_and_folds_them_when_narrow() {
        // Wide: More, Share, Downloads, Aa from the right, on a 32 pt
        // pitch, then the presence pill with its words; Keep in the field.
        let s = shell();
        let g = s.geo();
        let x = |r: Rect| (g.w - r.x()) as i32;
        assert_eq!((x(g.more), x(g.share.unwrap()), x(g.downloads), x(g.aa)), (40, 72, 104, 136));
        assert!(g.presence_label && g.presence.x() + g.presence.w() <= g.aa.x() - 8.0);
        assert!(g.where_.x() + g.where_.w() <= g.presence.x() - 12.0, "the field ends before the pill");
        assert!(g.keep.x() > g.where_.x() && g.keep.x() + g.keep.w() <= g.where_.x() + g.where_.w(), "Keep is inside the field");
        assert_eq!(s.more_rows()[0], MoreRow::OpenWith);
        // Narrower: the pill keeps only its icon.
        let mut s = shell();
        s.w = em(RAIL_NARROW) + 1.0 + em(BAR_PRESENCE_LABEL) - 10.0;
        let g = s.geo();
        assert!(!g.presence_label && g.presence.w() == em(PRESENCE_ICON) && g.share.is_some());
        // Narrower still: Reading and Share move into More; Back, the
        // chip's field and More stay.
        s.w = em(RAIL_NARROW) + 1.0 + em(BAR_TOOLS_FOLD) - 10.0;
        let g = s.geo();
        assert!(g.share.is_none() && g.aa == g.more);
        assert_eq!(&s.more_rows()[..2], &[MoreRow::Reading, MoreRow::Share]);
        assert!(g.where_.w() >= em(160.0) && g.more.x() + g.more.w() <= g.w);
    }

    #[test]
    fn downloads_list_what_was_answered_and_how_it_ended() {
        let mut s = shell();
        let url = s.store.trail().page().unwrap().url.clone();
        s.live = Some(LivePage::Shown(url));
        let dl = |id: u32, f: &str| Prompt::Download { id, file: f.into(), size: "1 KB".into(), origin: "atlas.example".into(), kind: "PDF".into() };
        assert!(s.ask(dl(7, "plan.pdf"), 0.0));
        assert!(s.answer_pressed(2, 600.0, Some([1; 16])), "Save");
        assert_eq!(s.downloads, vec![Download { id: 7, file: "plan.pdf".into(), size: "1 KB".into(), origin: "atlas.example".into(), state: DownloadState::Saving }]);
        assert!(s.downloads_new, "the button's dot");
        assert!(s.download_done(7, DownloadState::Saved("Downloads/plan.pdf".into())));
        assert!(!s.download_done(8, DownloadState::Failed("x".into())), "an answer for no download in the list");
        assert_eq!(s.downloads[0].state, DownloadState::Saved("Downloads/plan.pdf".into()));
        s.toggle_downloads();
        assert!(s.downloads_open && !s.downloads_new && s.geo().downloads_menu.is_some());
        assert!(s.escape() && !s.downloads_open);
        // Cancel is not a download; the list keeps the newest few.
        assert!(s.ask(dl(9, "a.pdf"), 1000.0));
        assert!(s.answer_pressed(0, 2000.0, None));
        assert_eq!(s.downloads.len(), 1);
        for i in 0..(DOWNLOADS_KEPT as u32 + 3) {
            s.note_download(100 + i, "f", "1 B", "o", DownloadState::Opening);
        }
        assert_eq!(s.downloads.len(), DOWNLOADS_KEPT);
        assert_eq!(s.downloads.last().unwrap().id, 100 + DOWNLOADS_KEPT as u32 + 2);
    }

    #[test]
    fn share_needs_a_page_shown_and_a_press_and_goes_once() {
        let mut s = shell();
        assert!(!s.share(Some([1; 16])), "no page from navigatord");
        let url = s.store.trail().page().unwrap().url.clone();
        let title = s.store.trail().page().unwrap().title.clone();
        s.live = Some(LivePage::Shown(url.clone()));
        assert!(!s.share(None), "no press, no sheet");
        assert!(s.share(Some([2; 16])));
        assert_eq!(s.outbox.pop(), Some(Out::Share { url: url.clone(), title, press: [2; 16] }));
        assert!(!s.share(Some([3; 16])), "one sheet at a time");
        s.shared("share: shared".into());
        assert!(!s.sharing);
        // From More, as a narrow window has it.
        s.toggle_more();
        assert!(s.more_open);
        s.press_more(MoreRow::Share, Some([4; 16]), 0.0);
        assert!(!s.more_open && s.sharing);
        assert!(matches!(s.outbox.pop(), Some(Out::Share { press: [4, ..], .. })));
    }

    #[test]
    fn a_sites_names_lose_the_marks_that_reorder_them() {
        assert_eq!(plain("invoice\u{202e}fdp.exe"), "invoicefdp.exe");
        assert_eq!(plain("a\u{2066}b\u{2069}\u{200f}c\u{feff}\n"), "abc");
        assert_eq!(plain("Ünïcödé · 東京.pdf"), "Ünïcödé · 東京.pdf");
        let mut s = shell();
        s.note_download(1, "x\u{202e}gpj.exe", "1 B", "o", DownloadState::Saving);
        assert_eq!(s.downloads[0].file, "xgpj.exe");
    }

    #[test]
    fn the_presence_pill_says_who_the_site_knows() {
        let mut s = shell();
        let site = crate::trail::site(&s.store.trail().page().unwrap().url).to_string();
        assert_eq!(s.presence(&site), Presence::NotKnown);
        s.store.set_knows(&site, true);
        assert_eq!(s.presence(&site), Presence::Remembers);
        s.accounts = Some((site.clone(), Accounts::Known(vec![navigator_control::Account { kind: navigator_control::AccountKind::Password, name: "ana".into() }])));
        assert_eq!(s.presence(&site), Presence::SignedIn("ana".into()));
        s.accounts = Some(("other.example".into(), Accounts::Known(vec![navigator_control::Account { kind: navigator_control::AccountKind::Password, name: "ana".into() }])));
        assert_eq!(s.presence(&site), Presence::Remembers, "another site's accounts say nothing here");
    }

    #[test]
    fn pinned_trails_are_tiles_and_find_narrows_the_rail() {
        let mut s = shell();
        let n = s.store.trails.len();
        assert!(n >= 3, "the stand-in store has trails");
        let cur = s.store.current;
        // Pin from More: the trail leaves Today for the tiles.
        s.toggle_more();
        assert!(s.more_rows().contains(&MoreRow::Pin));
        s.press_more(MoreRow::Pin, None, 0.0);
        assert!(s.store.trails[cur].pinned);
        let (pinned, today) = s.rail_trails();
        assert_eq!(pinned, vec![cur]);
        assert!(!today.contains(&cur) && today.len() == n - 1);
        assert!(s.more_rows().contains(&MoreRow::Unpin));
        // At most PINNED_MAX.
        for _ in 0..crate::trail::PINNED_MAX + 2 {
            let i = s.store.new_trail(false);
            s.store.follow(fake::spec("https://atlas.example/wiki/Impluvium"));
            s.store.toggle_pin(i);
        }
        assert_eq!(s.store.trails.iter().filter(|t| t.pinned).count(), crate::trail::PINNED_MAX);
        // Find in trails: typing narrows the rail; Enter shows the first;
        // Escape clears it.
        let mut s = shell();
        let title = s.store.trails[s.store.rail_order()[1]].title().to_lowercase();
        let word: String = title.split_whitespace().next().unwrap().chars().take(4).collect();
        s.begin_rail_find();
        assert!(s.rail_find_focus);
        for c in word.chars() {
            assert!(s.rail_find_key(Key::Char, Modifiers::default(), &c.to_string()));
        }
        let (_, today) = s.rail_trails();
        assert!(!today.is_empty() && today.iter().all(|&i| s.store.trails[i].matches(&word)));
        assert!(today.len() < s.store.trails.len(), "it narrows");
        s.rail_find_key(Key::Enter, Modifiers::default(), "");
        assert_eq!(s.store.current, today[0]);
        assert!(!s.rail_find_focus && s.rail_query() == word, "the filter stays after Enter");
        s.begin_rail_find();
        s.rail_find_key(Key::Escape, Modifiers::default(), "");
        assert!(s.rail_find.is_none() && s.rail_trails().1.len() == s.store.trails.len());
        // A press outside an empty field lets it go.
        s.begin_rail_find();
        s.press_at(s.w - 10.0, s.h - 10.0);
        assert!(!s.rail_find_focus && s.rail_find.is_none());
        // Nothing matches: nothing shown.
        s.begin_rail_find();
        for c in "zzqqxx".chars() {
            s.rail_find_key(Key::Char, Modifiers::default(), &c.to_string());
        }
        assert!(s.rail_trails().1.is_empty() && s.rail_earlier().is_empty());
    }

    #[test]
    fn monograms_are_two_letters_of_the_site() {
        assert_eq!(crate::ui::monogram_letters("atlas.example"), "At");
        assert_eq!(crate::ui::monogram_letters("x.example"), "X");
        assert_eq!(crate::ui::monogram_letters("über.example"), "Üb");
        assert_eq!(crate::ui::monogram_letters(""), "·");
    }

    #[test]
    fn start_puts_the_field_in_the_page_and_a_pinned_card_closes_the_empty_trail() {
        let mut s = shell();
        let wide = s.geo().where_;
        assert!(wide.y() < bar(), "on a page the field is the toolbar's");
        let pin = s.store.current;
        s.store.toggle_pin(pin);
        let n = s.store.trails.len();
        s.apply(Action::NewTrail, 0.0);
        assert!(s.on_start() && s.editing.is_some());
        let g = s.geo();
        assert!(g.where_.y() > bar() && g.where_.h() == em(START_FIELD_H), "on Start it is the page's");
        assert!(g.where_.x() > g.x0 && g.where_.x() + g.where_.w() < g.w, "centred in the page");
        if let Some(e) = &mut s.editing {
            e.edit.set_text("atr");
        }
        let g = s.geo();
        let d = g.drop.expect("suggestions");
        assert!(d.y() > g.where_.y() + g.where_.h(), "the suggestions hang under the field");
        assert_eq!(s.store.trails.len(), n + 1);
        let id = s.store.trails[pin].id;
        s.leave_start_for(pin);
        assert_eq!(s.store.trails.len(), n, "the empty trail closed");
        assert_eq!(s.store.trail().id, id);
        assert!(!s.on_start());
    }
}
