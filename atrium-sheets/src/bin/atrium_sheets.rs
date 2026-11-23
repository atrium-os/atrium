//! atrium-sheets: the spreadsheet app (docs/spec/officina.md §8, O2).
//!
//!   atrium-sheets                 the launcher when started by Portcullis
//!   atrium-sheets FILE            open FILE by path (development; apps get files
//!                                 from the picker)
//!   atrium-sheets --bench PATTERN ROWS SECS [HZ]
//!                                 the O2 scroll gate, live: a ROWS-row sheet driven by
//!                                 the scripted PATTERN (swipe, diagonal, wheel, page,
//!                                 jump, or all) on the wall clock; logs one report line
//!                                 per pattern and exits
//!
//! Open asks Scrinium to launch the picked workbook in its own jail. Save
//! sends the encoded revision to Scrinium. Save As obtains a new workbook grant.
//! The window uses FRESCO_SOCKET and reconnects to LIMEN_SOCKET across broker restarts.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_sheets::app::{Cmd, Sheets, Ui, PICKER_SLOT};
use pergola::print::{Orientation, Outcome, Paper, Printing, Step};
use atrium_sheets::bench::{self, Frame, Pattern, Report};
use atrium_sheets::doc::{self, Doc};
use atrium_sheets::files::{format_of, stem_of, Format};
use officina_convert::launch::ConvertError;
use limen::picker::{Filter, Kind, ToChild, ToParent};
use limen::proto::ParentEvent;
use pergola::embed::Embeds;
use pergola::geom::Point;
use pergola::widgets::{GridState, ScrollState};
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};

/// The largest file Open reads.
const MAX_FILE: u64 = 2 << 30;
/// How often the journal is made durable.
const SYNC_EVERY: Duration = Duration::from_secs(2);

/// Where the user's file is, for Save.
enum Target {
    /// A Scrinium grant for replacing a picked workbook.
    Picked { commit: [u8; 16], format: Format },
    /// A path supplied for development.
    Path { path: PathBuf, format: Format },
}

/// What the picker is up for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asking {
    Open,
    SaveAs,
    Commit,
    LinkedPicture,
    /// The copy to compare with.
    Compare,
    MergeBase,
    MergeIncoming,
    Versions,
    Recent,
}

fn state_root() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("atrium-sheets/autosave")
}

fn new_autosave_dir(root: &Path) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    root.join(format!("{}-{n}", std::process::id()))
}

fn read_all(f: &mut File) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.take(MAX_FILE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_FILE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the file is over 2 GiB"));
    }
    Ok(data)
}

/// The file's bytes in `format`; a protected document's encrypted with its
/// password (CSV has no way to carry one and is written as is).
fn encode(d: &Doc, format: Format) -> io::Result<Vec<u8>> {
    let protect = |kind: &str, b: Vec<u8>| match &d.protection {
        Some(p) => officina_convert::launch::protect(kind, &b, &p.password).map_err(|e| io::Error::other(e.to_string())),
        None => Ok(b),
    };
    match format {
        Format::Native => d.stored_native(),
        Format::Csv => d.write_csv(Vec::new()),
        Format::Xlsx => protect("xlsx", officina_convert::xlsx::write(&d.wb, Vec::new())?),
        Format::Xlsm => protect("xlsm", officina_convert::xlsx::write_xlsm(&d.wb, Vec::new())?),
        Format::Ods => protect("ods", officina_convert::ods::write(&d.wb, Vec::new())?),
        Format::Xls => Err(io::Error::other("Sheets doesn't write Excel 97–2003 files")),
    }
}

fn saved_status(d: &Doc, format: Format) -> String {
    let lost = format == Format::Csv && d.wb.groups().next().is_some();
    // Macros go back only into their own kind of file (§5.3).
    let fam = d.wb.macros().map(|m| m.family);
    let macros_lost = match (fam, format) {
        (None, _) => false,
        (Some(officina_grid::macros::MacroFamily::Ooxml), Format::Xlsm) | (Some(officina_grid::macros::MacroFamily::Odf), Format::Ods) | (Some(_), Format::Native) => false,
        _ => true,
    };
    let mut status = if lost {
        let label = if d.csv_sep == b'\t' { "TSV" } else { "CSV" };
        format!("Saved “{}”. {label} keeps formula results as values.", d.name)
    } else if macros_lost {
        format!("Saved “{}”. Its macros aren't in this file: this format can't hold them.", d.name)
    } else {
        format!("Saved “{}”.", d.name)
    };
    if matches!(format, Format::Xlsx | Format::Xlsm) && d.wb.pivots().iter().any(|p| !p.groups.is_empty()) {
        status.push_str(" Excel export saved grouped ODS pivot tables as cell values.");
    }
    if matches!(format, Format::Xlsx | Format::Xlsm) && d.wb.pivots().iter().any(|p| {
        p.rows.iter().chain(&p.cols).any(|f| matches!(f.sort, officina_grid::pivot::AxisSort::Data { .. }))
    }) {
        status.push_str(" Excel export uses label sort for ODS pivot fields sorted by values.");
    }
    if format == Format::Ods {
        use officina_grid::formula::ast::Expr;

        let mut changes = Vec::new();
        if d.wb.pivots().iter().any(|p| p.data.iter().any(|f| f.show_as.is_excel_extension())) {
            changes.push("saved pivot tables with Excel-only display calculations as cell values");
        }
        if d.wb.groups().any(|(_, group)| {
            let mut referenced = false;
            group.expr.walk(&mut |expr| referenced |= matches!(expr, Expr::Spill(_)));
            referenced
        }) {
            changes.push("replaced spill references with fixed ranges or reference errors");
        }
        let tables = d.wb.tables();
        if !tables.is_empty() && d.wb.groups().any(|(_, group)| {
            let mut referenced = false;
            group.expr.walk(&mut |expr| {
                referenced |= match expr {
                    Expr::Table(_) => true,
                    Expr::Name(name) => d.wb.names().get(group.sheet, name).is_none() && d.wb.table(name).is_some(),
                    _ => false,
                };
            });
            referenced
        }) {
            changes.push("converted table references to cell ranges");
        }
        if tables.iter().any(|table| table.style.is_some() || table.stripes) {
            changes.push("omitted table styles");
        }
        if tables.iter().any(|table| table.totals) {
            changes.push("kept table totals in plain cells");
        }
        if d.wb.charts().iter().flat_map(|chart| &chart.series).any(|series| {
            matches!(&series.name, Some(officina_grid::charts::ChartText::Lit(name)) if !name.is_empty())
        }) {
            changes.push("omitted typed chart series names");
        }
        if (0..d.wb.sheet_count() as u16).any(|sheet| {
            d.wb.comments(sheet).values().any(|comment| !comment.thread().is_empty())
        }) {
            changes.push("saved comment threads as plain notes");
        }
        if !changes.is_empty() {
            status.push_str(&format!(" ODS {}.", changes.join(", ")));
        }
    }
    status
}

/// Where Save writes a document opened from `target`: nowhere for an `.xls`,
/// which Sheets reads but doesn't write (Save asks where to keep a copy).
fn keep_target(d: &mut Doc, target: Option<Target>) -> Option<Target> {
    match target {
        Some(Target::Picked { format: Format::Xls, .. } | Target::Path { format: Format::Xls, .. }) => {
            d.status = format!("{} Sheets reads Excel 97–2003 files but doesn't write them: Save asks where to keep a copy.", d.status);
            None
        }
        t => t,
    }
}

/// In-place write for the development command-line path.
fn write_over(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn save_name(name: &str) -> String {
    let stem: String = stem_of(name).chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let stem = stem.trim_start_matches('.');
    let stem = if stem.is_empty() { "Sheet" } else { stem };
    let mut end = stem.len().min(200);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}.osheet", &stem[..end])
}

fn workbook_broker(request: &portcullis_ipc::Request, seconds: u64) -> Result<portcullis_ipc::Response, String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION, WORKERS_SOCKET_PATH};
    use std::os::unix::net::UnixStream;
    let mut socket = UnixStream::connect(WORKERS_SOCKET_PATH).map_err(|e| e.to_string())?;
    let timeout = Some(Duration::from_secs(seconds));
    socket.set_read_timeout(timeout).and_then(|_| socket.set_write_timeout(timeout)).map_err(|e| e.to_string())?;
    match round_trip(&mut socket, &Request::Hello { version: PROTO_VERSION }).map_err(|e| e.to_string())? {
        Response::Hello { .. } => {}
        response => return Err(format!("The workbook broker answered {response:?}")),
    }
    round_trip(&mut socket, request).map_err(|e| e.to_string())
}

fn request_workbook(recover: bool, selected: Option<[u8; 16]>, activation: Option<fresco_protocol::activation::Token>) -> Result<Vec<portcullis_ipc::PendingWorkbook>, String> {
    use portcullis_ipc::{Request, Response};
    let activation = activation.ok_or("Use the workbook button in the Sheets window")?;
    let request = match selected {
        Some(id) => Request::RecoverSelectedWorkbook { activation, id },
        None if recover => Request::RecoverWorkbook { activation },
        None => Request::NewWorkbook { activation },
    };
    match workbook_broker(&request, if recover { 5 } else { 2 })? {
        Response::Opened { .. } => Ok(Vec::new()),
        Response::PendingWorkbooks { books } if recover && books.len() <= 8 => Ok(books),
        Response::Error { message } => Err(message),
        response => Err(format!("The workbook broker answered {response:?}")),
    }
}


/// A protected file waiting for its password.
enum Locked {
    File { name: String, data: Vec<u8>, target: Option<Target> },
    /// A protected document's autosave folder, to restore.
    Recovery { dir: PathBuf, name: String },
}

struct Run {
    recovery_id: Option<[u8; 16]>,
    sheets: Sheets,
    target: Option<Target>,
    locked: Option<Locked>,
    root: PathBuf,
    /// A file being read (converted, loaded and recalculated) on its own
    /// thread, so the window keeps answering however big the file is.
    opening: Option<Opening>,
    merge_base: Option<officina_grid::compare::CellSnapshot>,
}

struct Opening {
    name: String,
    /// A comparison or merge input instead of a workbook to open.
    review: Option<Asking>,
    target: Option<Target>,
    read_only: bool,
    loading: atrium_sheets::files::Loading,
}

impl Run {
    fn set_status(&self, s: impl Into<String>) {
        self.sheets.doc.borrow_mut().status = s.into();
        self.sheets.changed();
    }

    /// Puts `d` on screen with its own autosave folder (a restored one keeps its own). The document it replaces
    /// closes cleanly if saved; with unsaved changes its autosave folder stays, and
    /// it comes back at the next start.
    fn show(&mut self, mut d: Doc, target: Option<Target>) {
        let target = keep_target(&mut d, target);
        if d.autosave().is_none() {
            if let Err(e) = d.autosave_to(&new_autosave_dir(&self.root)) {
                d.status = format!("{} Autosave is off: {e}.", d.status);
            }
        }
        self.merge_base = None;
        let old = self.sheets.replace(d);
        self.target = target;
        if old.dirty {
            let name = old.name.clone();
            let mut old = old;
            if let Some(a) = old.autosave() {
                let _ = a.sync();
            }
            drop(old);
            let s = self.sheets.doc.borrow().status.clone();
            self.set_status(format!("{s} Unsaved changes to “{name}” are kept and come back next time Sheets starts."));
        } else {
            let _ = old.close();
        }
    }

    fn save(&mut self) -> bool {
        let Some(Target::Path { path, format }) = self.target.as_ref() else { return false };
        let format = *format;
        if format != Format::Native && self.sheets.doc.borrow().wb.tracked_cells().is_some() {
            self.set_status("Choose where to save an .osheet copy with its tracked cell changes.");
            return false;
        }
        // A file keeps every value as it should be: finish recalculating first.
        { let mut d = self.sheets.doc.borrow_mut(); if d.wb.recalc_pending() { d.wb.recalc(); } }
        let r = {
            let d = self.sheets.doc.borrow();
            encode(&d, format).and_then(|bytes| {
                let mut file = OpenOptions::new().write(true).open(path)?;
                write_over(&mut file, &bytes)
            })
        };
        let mut d = self.sheets.doc.borrow_mut();
        match r.and_then(|_| d.saved()) {
            Ok(()) => {
                d.status = saved_status(&d, format);
            }
            Err(e) => d.status = format!("Couldn't save “{}”: {e}. Your changes are still here and in the autosave.", d.name),
        }
        drop(d);
        self.sheets.changed();
        true
    }

    fn published(&mut self, name: String, commit: [u8; 16], edits: u64) {
        self.target = Some(Target::Picked { commit, format: Format::Native });
        let mut doc = self.sheets.doc.borrow_mut();
        doc.name = name.clone();
        doc.status = if doc.edits == edits {
            match doc.saved() {
                Ok(()) => format!("Saved as “{name}”."),
                Err(e) => format!("Saved as “{name}”, but couldn't update autosave: {e}."),
            }
        } else {
            format!("Saved as “{name}”. More recent edits are still pending.")
        };
        // A publication acknowledgement changes the name even if another edit
        // arrived while the picker was writing. Keep the original recovery ID.
        if let Some(autosave) = doc.autosave() {
            if let Err(e) = autosave.set_display_name(&name) {
                doc.status.push_str(&format!(" Couldn't update the autosave name: {e}."));
            }
        }
        if let Some(id) = self.recovery_id {
            use portcullis_ipc::{Request, Response};
            let result = workbook_broker(&Request::NameWorkbookRecovery { id, name }, 2);
            let error = match result {
                Ok(Response::Ok) => None,
                Ok(Response::Error { message }) | Err(message) => Some(message),
                Ok(other) => Some(format!("{other:?}")),
            };
            if let Some(error) = error {
                doc.status.push_str(&format!(" Couldn't update the recovery chooser: {error}."));
            }
        }
        drop(doc);
        self.sheets.changed();
    }

    /// A picked file arrived.
    fn opened(&mut self, name: String, write: bool, fd: std::os::fd::OwnedFd, commit: Option<[u8; 16]>) {
        let mut f = File::from(fd);
        let format = format_of(&name);
        match read_all(&mut f) {
            Ok(data) => self.open_bytes(name, data, commit.filter(|_| write).map(|commit| Target::Picked { commit, format }), !write, None),
            Err(e) => self.set_status(format!("Couldn't read “{name}”: {e}")),
        }
    }

    /// Compares the window's workbook with the picked copy, read on its own
    /// thread as an open is.
    fn read_review(&mut self, name: String, fd: std::os::fd::OwnedFd, purpose: Asking) {
        let mut f = File::from(fd);
        match read_all(&mut f) {
            Ok(data) => {
                self.opening = None;
                self.open_bytes(name, data, None, true, None);
                if let Some(o) = &mut self.opening {
                    o.review = Some(purpose);
                }
            }
            Err(e) => self.set_status(format!("Couldn't read review copy “{name}”: {e}")),
        }
    }

    /// Opens `data` (named `name`), with `password` when it is protected;
    /// without the right one, the password bar asks and the file waits.
    fn open_bytes(&mut self, name: String, data: Vec<u8>, target: Option<Target>, read_only: bool, password: Option<&str>) {
        let loading = match atrium_sheets::files::Loading::start(&name, data, password) {
            Ok(l) => l,
            Err(e) => {
                self.set_status(format!("Couldn't open “{name}”: {e}"));
                return;
            }
        };
        // A file still opening is dropped for this one (its thread finishes unheard).
        self.set_status(format!("Reading “{name}”…"));
        self.opening = Some(Opening { name, review: None, target, read_only, loading });
    }

    /// The file being opened, if its thread is done: on screen, or the
    /// password asked for, or why not. True while one is still opening.
    fn poll_open(&mut self, want_picker: &mut Option<Asking>) -> bool {
        let Some(o) = &self.opening else { return false };
        let Some((r, data)) = o.loading.poll() else { return true };
        let Opening { name, review, target, read_only, .. } = self.opening.take().unwrap();
        if let Some(purpose) = review {
            match (purpose, r) {
                (Asking::Compare, Ok(d)) => self.sheets.compare_with(&name, d.wb),
                (Asking::MergeBase, Ok(d)) => {
                    self.merge_base = Some(officina_grid::compare::CellSnapshot::capture(&d.wb));
                    *want_picker = Some(Asking::MergeIncoming);
                }
                (Asking::MergeIncoming, Ok(d)) => {
                    if let Some(base) = self.merge_base.take() {
                        self.sheets.merge_copies(&name, &base, &d.wb);
                    } else {
                        self.set_status("The common base is no longer available. Run Merge Copies again.");
                    }
                }
                (_, Err(ConvertError::NeedsPassword | ConvertError::WrongPassword)) => {
                    self.merge_base = None;
                    self.set_status(format!("“{name}” is protected. Use an unprotected review copy."));
                }
                (_, Err(ConvertError::Failed(e))) => {
                    self.merge_base = None;
                    self.set_status(format!("Couldn't read review copy “{name}”: {e}"));
                }
                _ => unreachable!("only review purposes reach this branch"),
            }
            return false;
        }
        match r {
            Ok(mut d) => {
                if read_only {
                    if !d.status.is_empty() {
                        if !d.status.ends_with(['.', '!', '?']) { d.status.push('.'); }
                        d.status.push(' ');
                    }
                    d.status.push_str("Opened read-only. Save asks where to put a copy.");
                }
                self.show(d, target);
            }
            Err(e @ (ConvertError::NeedsPassword | ConvertError::WrongPassword)) => {
                let again = e == ConvertError::WrongPassword;
                self.sheets.ask_password(&if again { format!("Wrong password for “{name}”. Try again") } else { format!("“{name}” is protected. Its password") }, false);
                self.locked = Some(Locked::File { name, data, target });
            }
            Err(ConvertError::Failed(e)) => self.set_status(e),
        }
        false
    }

    /// Restores a protected document's autosave folder, asking for its
    /// password.
    fn recover_locked(&mut self, dir: PathBuf, name: String, password: Option<&str>) {
        match password.map(|pw| doc::recover_with(&dir, Some(pw))) {
            Some(Ok((mut d, rec))) => {
                d.status = format!("Restored “{name}” as it was before Sheets last stopped ({} changes since its last save).", rec.actions);
                self.show(d, None);
            }
            Some(Err(e)) if e.kind() != io::ErrorKind::PermissionDenied => self.set_status(format!("Couldn't restore “{name}”: {e}")),
            r => {
                let again = r.is_some();
                self.sheets.ask_password(&if again { format!("Wrong password for “{name}”. Try again") } else { format!("Restore “{name}”: its password") }, false);
                self.locked = Some(Locked::Recovery { dir, name });
            }
        }
    }

    /// The password bar's answer: a waiting file opened, or the document's
    /// password set.
    fn password_done(&mut self) {
        let (bar, text) = self.sheets.take_password();
        if bar.setting {
            let pw = (!text.is_empty()).then_some(text.as_str());
            let r = self.sheets.doc.borrow_mut().set_password(pw);
            let mut d = self.sheets.doc.borrow_mut();
            d.edits += 1;
            d.dirty = true;
            d.status = match (r, pw) {
                (Err(e), _) => format!("The password is set, but autosave stopped: {e}."),
                (Ok(()), Some(_)) => format!("“{}” is protected with a password from its next save. CSV files can't carry one.", d.name),
                (Ok(()), None) => format!("“{}” has no password from its next save.", d.name),
            };
            drop(d);
            self.sheets.changed();
            return;
        }
        match self.locked.take() {
            Some(Locked::File { name, data, target }) => self.open_bytes(name, data, target, false, Some(&text)),
            Some(Locked::Recovery { dir, name }) => self.recover_locked(dir, name, Some(&text)),
            None => {}
        }
    }

    fn password_cancelled(&mut self) {
        match self.locked.take() {
            Some(Locked::File { name, .. }) => self.set_status(format!("“{name}” stayed closed.")),
            Some(Locked::Recovery { name, .. }) => self.set_status(format!("“{name}” wasn't restored; it comes back next time Sheets starts.")),
            None => {}
        }
    }
}

fn receive_workbook() -> io::Result<Option<atrium_open::Opened>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    let fd = std::io::stdin().as_fd().try_clone_to_owned()?;
    atrium_open::Channel::from_stream(UnixStream::from(fd)).recv(Some(Duration::from_secs(5)))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--bench") {
        std::process::exit(bench_main(&args[2..]));
    }

    let workbook = if args.get(1).is_some_and(|arg| arg == "--document") {
        let opened = receive_workbook();
        match opened {
            Ok(Some(file)) => Some(file),
            other => {
                eprintln!("atrium-sheets: missing workbook launch descriptor: {:?}", other.err());
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let recover = args.get(1).is_some_and(|arg| arg == "--recover-document");
    let dedicated = recover || workbook.is_some() || args.get(1).is_some_and(|arg| arg == "--blank-document");
    officina_convert::pictures::install();
    let private = Path::new("/atrium/recovery");
    if recover && !private.is_dir() {
        eprintln!("atrium-sheets: missing private recovery mount");
        std::process::exit(1);
    }
    let root = if dedicated && private.is_dir() { private.to_owned() } else { state_root() };
    // The custom lists fills follow, kept beside the autosaves.
    if let Some(dir) = state_root().parent() {
        atrium_sheets::lists::init(dir);
    }
    let _ = fs::create_dir_all(&root);
    // A document a crash left comes back first; it, or a file named on the
    // command line, is read on its own thread once the window shows.
    let mut locked: Option<(PathBuf, String)> = None;
    let mut restore: Option<(PathBuf, String)> = None;
    let mut start_file: Option<(String, Vec<u8>, Target)> = None;
    if let Some((dir, name)) = (!dedicated || recover).then(|| doc::pending(&root).pop()).flatten() {
        if doc::recovery_locked(&dir) {
            locked = Some((dir, name));
        } else {
            restore = Some((dir, name));
        }
    }
    if !dedicated && restore.is_none() && locked.is_none() {
        if let Some(path) = args.get(1) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
            match fs::read(&p) {
                Ok(data) => start_file = Some((name.clone(), data, Target::Path { path: p.clone(), format: format_of(&name) })),
                Err(e) => eprintln!("atrium-sheets: {e}"),
            }
        }
    }
    let launcher = args.len() == 1 && restore.is_none() && locked.is_none()
        && Path::new(portcullis_ipc::WORKERS_SOCKET_PATH).exists();
    let (doc, target) = (Doc::new("Untitled"), None);

    let mut win = match Window::open(&WindowDesc::new("Sheets", SizeSpec::Fixed(1100, 720))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("atrium-sheets: frescod: {e}");
            std::process::exit(1);
        }
    };
    let mut embeds = None;
    let mut next_limen_attempt = Instant::now();
    let dirty = Arc::new(AtomicBool::new(true));
    let mut sheets = Sheets::new(
        Doc::new(if recover { "Pending workbook" } else { "Untitled" }),
        Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        Mutable::with_dirty(GridState::default(), dirty.clone()),
        Mutable::with_dirty(Ui::default(), dirty.clone()),
        Mutable::with_dirty(0u64, dirty.clone()),
        (win.width, win.height),
    );
    sheets.launcher = launcher;
    let recovery_id = args.get(2).filter(|_| dedicated).and_then(|arg| arg.parse::<u128>().ok()).map(u128::to_be_bytes);
    let mut run = Run { recovery_id, sheets, target: None, locked: None, root: root.clone(), opening: None, merge_base: None };
    if !launcher && !recover {
        run.show(doc, target);
    }
    if let Some(file) = workbook {
        eprintln!("atrium-sheets: “{}” opened in its document jail{}", file.name,
                  if file.write { " (write)" } else { "" });
        run.opened(file.name, file.write, file.file.into(), file.commit);
    } else if let Some((dir, name)) = locked {
        run.recover_locked(dir, name, None);
    } else if let Some((dir, name)) = restore {
        match atrium_sheets::files::Loading::restore(dir, &name) {
            Ok(loading) => {
                run.set_status(format!("Restoring “{name}”…"));
                run.opening = Some(Opening { name, review: None, target: None, read_only: false, loading });
            }
            Err(e) => eprintln!("atrium-sheets: couldn't restore {name}: {e}"),
        }
    } else if recover {
        run.set_status("The recovery volume contains no complete workbook. Close and choose Don’t Save to discard it.");
    } else if let Some((name, data, t)) = start_file {
        run.open_bytes(name, data, Some(t), false, None);
    }
    let size = run.sheets.size.clone();
    let mut app = App::new_with_flag(run.sheets.clone(), dirty);
    let _ = win.paint(&mut app);

    let mut asking: Option<Asking> = None;
    let mut embed = None;
    let mut want_picker: Option<Asking> = None;
    let mut last_sync = Instant::now();
    // A close intent belongs to the active save and expires when it finishes.
    let mut close_after_save = false;
    let mut staged_save = None;
    let mut discard = false;
    let mut recalculating = false;
    // Printing: the sheet as it was asked for, the PDF being made (true once
    // the print sheet is up: the answer to its Relayout), and the sheet.
    let mut print_snap: Option<Arc<atrium_sheets::print::Snapshot>> = None;
    let mut print_pdf: Option<(std::sync::mpsc::Receiver<Result<Vec<u8>, String>>, bool)> = None;
    let mut printing: Option<Printing> = None;
    let mut want_print = false;
    'main: loop {
        if embeds.is_none() && Instant::now() >= next_limen_attempt {
            match Embeds::connect() {
                Ok(connection) => {
                    win.set_slot_binder(Embeds::binder(&connection));
                    embeds = Some(connection);
                }
                Err(_) => next_limen_attempt = Instant::now() + Duration::from_secs(1),
            }
        }
        // While a recalculation is unfinished, a slice of it each turn (the
        // window answers in between) and no waiting for input.
        let opening = run.poll_open(&mut want_picker);
        let wait = if recalculating { 1 } else if asking.is_some() || opening { 20 } else { 250 };
        let wait = if printing.is_some() || print_pdf.is_some() { wait.min(20) } else { wait };
        let mut close_requested = false;
        let stepped = win.step_with(&mut app, Some(Duration::from_millis(wait)), |ev| match ev {
            fresco_client::Event::Key { hid_usage: 0x13, pressed: true, modifiers, repeat: false, .. } if modifiers & 0b0001_0001 != 0 => {
                want_print = true;
                true
            }
            fresco_client::Event::Key { hid_usage: 0x1a, pressed: true, modifiers, repeat: false, .. } if modifiers & 0b0001_0001 != 0 => {
                close_requested = true;
                true
            }
            _ => false,
        });
        if stepped.is_err() { break; }
        if close_requested || matches!(stepped, Ok(StepResult::Closed)) {
            let needs_decision = {
                let mut doc = run.sheets.doc.borrow_mut();
                doc.dirty || (recover && doc.autosave().is_none())
            };
            if matches!(asking, Some(Asking::Commit | Asking::SaveAs)) {
                close_after_save = true;
            } else if needs_decision && asking.is_none() && !run.sheets.closing.get() {
                run.sheets.closing.set(true);
                run.sheets.changed();
            } else {
                break;
            }
        }
        {
            let mut d = run.sheets.doc.borrow_mut();
            d.recalc_budget = Some(atrium_sheets::doc::RECALC_SLICE);
            // A grid edit can leave work that finishes in this first slice.
            // Its final values and conditional formats still need a new frame.
            let was = recalculating || d.wb.recalc_pending();
            recalculating = d.settle();
            drop(d);
            if was || recalculating {
                run.sheets.changed();
            }
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
            app.invalidate();
        }
        for c in run.sheets.take_cmds() {
            match c {
                Cmd::NewWorkbook(activation) | Cmd::RecoverWorkbook(_, activation) => {
                    let selected = match c { Cmd::RecoverWorkbook(id, _) => id, _ => None };
                    let result = request_workbook(matches!(c, Cmd::RecoverWorkbook(..)), selected, activation);
                    let previous = run.sheets.pending_books.get_cloned();
                    run.sheets.pending_books.set(Vec::new());
                    match result {
                        Ok(mut books) if !books.is_empty() => {
                            let starting: Vec<_> = books.iter().filter(|b| b.starting)
                                .map(|b| format!("“{}”", b.name)).collect();
                            // A selected launch replies only for its own ID. Keep the
                            // other chooser rows; the broker revalidates each selection.
                            if let Some(id) = selected {
                                if books.iter().any(|b| b.id == id && b.starting) {
                                    books.extend(previous.into_iter().filter(|b| b.id != id));
                                }
                            }
                            books.retain(|b| !b.starting);
                            run.sheets.pending_books.set(books);
                            if starting.is_empty() {
                                run.set_status("Choose a workbook to recover.");
                            } else {
                                run.set_status(format!("Sheets is still starting recovery for {}.", starting.join(", ")));
                            }
                        }
                        Ok(_) => run.set_status(""),
                        Err(why) => run.set_status(why),
                    }
                }
                Cmd::Open => {
                    run.sheets.pending_books.set(Vec::new());
                    want_picker = Some(Asking::Open);
                }
                Cmd::RecentWorkbooks => {
                    run.sheets.pending_books.set(Vec::new());
                    want_picker = Some(Asking::Recent);
                }
                Cmd::Data(atrium_sheets::data::DataCmd::CompareWith) => want_picker = Some(Asking::Compare),
                Cmd::Data(atrium_sheets::data::DataCmd::BrowseVersions) => {
                    if matches!(run.target, Some(Target::Picked { .. })) {
                        want_picker = Some(Asking::Versions);
                    } else {
                        run.set_status("Open or save a workbook through Scrinium to browse its versions.");
                    }
                }
                Cmd::Data(atrium_sheets::data::DataCmd::MergeCopies) => {
                    run.merge_base = None;
                    want_picker = Some(Asking::MergeBase);
                }
                Cmd::PasswordDone => run.password_done(),
                Cmd::PasswordClose if recover && matches!(run.locked, Some(Locked::Recovery { .. })) => break 'main,
                Cmd::PasswordClose => run.password_cancelled(),
                Cmd::Save | Cmd::CloseSave if matches!(asking, Some(Asking::Commit | Asking::SaveAs)) && close_after_save => {}
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::Print => want_print = true,
                Cmd::Save | Cmd::CloseSave if matches!(run.target, Some(Target::Picked { .. })) => {
                    close_after_save = matches!(c, Cmd::CloseSave);
                    let native_copy = matches!(run.target, Some(Target::Picked { format, .. }) if format != Format::Native)
                        && run.sheets.doc.borrow().wb.tracked_cells().is_some();
                    want_picker = Some(if native_copy { Asking::SaveAs } else { Asking::Commit });
                }
                Cmd::Save => {
                    if !run.save() {
                        want_picker = Some(Asking::SaveAs);
                    }
                }
                Cmd::CloseSave => {
                    if run.save() {
                        if !run.sheets.doc.borrow().dirty {
                            break 'main;
                        }
                    } else {
                        want_picker = Some(Asking::SaveAs);
                        close_after_save = true;
                    }
                }
                Cmd::CloseDiscard => {
                    if recover && run.sheets.doc.borrow_mut().autosave().is_none() {
                        // An interrupted first generation or unreadable base.
                        // This process has only its own slot mounted here.
                        let clear = || -> io::Result<()> {
                            for entry in fs::read_dir(private)? {
                                let entry = entry?;
                                if entry.file_type()?.is_dir() {
                                    fs::remove_dir_all(entry.path())?;
                                } else {
                                    fs::remove_file(entry.path())?;
                                }
                            }
                            Ok(())
                        };
                        if let Err(e) = clear() {
                            run.set_status(format!("Couldn't discard recovery: {e}"));
                            continue;
                        }
                    }
                    discard = true;
                    break 'main;
                }
                Cmd::CloseCancel | Cmd::Undo | Cmd::Redo | Cmd::RemoveMacros | Cmd::Data(_) | Cmd::PasswordOpen | Cmd::ChartKind(_) | Cmd::ChartLegend(_) | Cmd::ChartGroup(_) | Cmd::ChartTitle | Cmd::ChartDelete | Cmd::ChartDone | Cmd::FilterToggle(_) | Cmd::FilterAll(_) | Cmd::FilterApply | Cmd::FilterCancel | Cmd::ListPick(_) | Cmd::ListCancel | Cmd::Valid(_) | Cmd::UseSelection | Cmd::FilterOpen(_) | Cmd::Cond(_) | Cmd::Pivot(_) | Cmd::Review(_) | Cmd::Tab(_) | Cmd::FillOptions | Cmd::ListsForget(_) | Cmd::ListsClose => {}
            }
        }
        if std::mem::take(&mut want_print) {
            if asking.is_some() || printing.is_some() || print_pdf.is_some() {
                run.set_status("Finish what's open first, then print.");
            } else if embeds.is_none() || win.embed_token().is_none() {
                run.set_status("Printing isn't available here.");
            } else {
                match atrium_sheets::print::snapshot(&run.sheets.doc) {
                    Ok(snap) => {
                        run.set_status("Getting the sheet ready to print…");
                        let snap = Arc::new(snap);
                        if snap.next_link().is_some() {
                            want_picker = Some(Asking::LinkedPicture);
                        } else {
                            print_pdf = Some((sheet_pdf(snap.clone(), Paper::A4), false));
                        }
                        print_snap = Some(snap);
                    }
                    Err(why) => run.set_status(format!("Couldn't print: {why}.")),
                }
            }
        }
        if let Some((rx, again)) = &print_pdf {
            let again = *again;
            match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                r => {
                    print_pdf = None;
                    let r = r.map_err(|_| "the layout stopped".to_string()).and_then(|r| r);
                    let title = print_snap.as_ref().map(|s| stem_of(&run.sheets.doc.borrow().name).to_string() + if s.name.is_empty() { "" } else { " – " } + &s.name).unwrap_or_default();
                    let r = match (r, &embeds, win.embed_token(), again) {
                        (Ok(pdf), Some(e), _, true) => printing.as_mut().map(|p| p.document_bytes(e, &pdf).map_err(|e| format!("{e:?}"))).unwrap_or(Ok(())),
                        (Ok(pdf), Some(e), Some(token), false) => Printing::start_bytes(e, token, PICKER_SLOT, &pdf, &title, true, Some(Paper::A4))
                            .map(|p| {
                                printing = Some(p);
                                run.set_status("");
                                run.sheets.ui.set(Ui { asking: true });
                            })
                            .map_err(|e| format!("the print sheet didn't open ({e:?})")),
                        (Ok(_), ..) => Err("Limen isn't running".into()),
                        (Err(e), ..) => Err(e),
                    };
                    if let Err(why) = r {
                        run.set_status(format!("Couldn't print: {why}."));
                        if let (Some(p), Some(e)) = (printing.take(), &embeds) {
                            p.cancel(e);
                        }
                        print_snap = None;
                        run.sheets.ui.set(Ui { asking: false });
                    }
                }
            }
        }
        if let Some(what) = want_picker.take_if(|_| asking.is_none() && printing.is_none()) {
            match (&embeds, win.embed_token()) {
                (Some(e), Some(token)) => {
                    let name = run.sheets.doc.borrow().name.clone();
                    let filter = match what {
                        Asking::Recent => Filter::file(),
                        Asking::Open => Filter { kind: Kind::Workbook, extensions: ["xlsx", "xlsm", "xls", "ods", "csv", "tsv", "txt", "osheet"].map(String::from).to_vec(), multiple: false, write: true, title: "Open a spreadsheet".into(), name: String::new(), origin: String::new() },
                        Asking::Compare | Asking::MergeBase | Asking::MergeIncoming => Filter {
                            kind: Kind::File,
                            extensions: ["xlsx", "xlsm", "xls", "ods", "csv", "tsv", "txt", "osheet"].map(String::from).to_vec(),
                            multiple: false, write: false,
                            title: match what {
                                Asking::MergeBase => "Choose the common base workbook".into(),
                                Asking::MergeIncoming => "Choose the incoming workbook".into(),
                                _ => format!("Compare “{}” with…", stem_of(&name)),
                            },
                            name: String::new(), origin: String::new(),
                        },
                        Asking::Versions => Filter::file(),
                        Asking::SaveAs | Asking::Commit => Filter::save(&save_name(&name), ""),
                        Asking::LinkedPicture => {
                            let Some(link) = print_snap.as_ref().and_then(|snap| snap.next_link()) else { continue };
                            Filter {
                                kind: Kind::File,
                                extensions: ["png", "jpg", "jpeg"].map(String::from).to_vec(),
                                multiple: false, write: false,
                                title: format!("Choose the linked picture \u{201c}{}\u{201d}", link.rsplit(['/', '\\']).next().unwrap_or(link).chars().take(80).collect::<String>()),
                                name: String::new(), origin: String::new(),
                            }
                        }
                    };
                    let commit_target = match (&run.target, what) {
                        (Some(Target::Picked { commit, format }), Asking::Commit) => Some((*commit, *format)),
                        _ => None,
                    };
                    let source = if matches!(what, Asking::SaveAs | Asking::Commit) {
                        let mut d = run.sheets.doc.borrow_mut();
                        if d.wb.recalc_pending() { d.wb.recalc(); }
                        match encode(&d, commit_target.map_or(Format::Native, |(_, format)| format)).and_then(|bytes| {
                            let file = limen::fdpass::sealed_file("workbook.osheet", &bytes)?;
                            Ok((file, bytes.len() as u64))
                        }) {
                            Ok((file, size)) => { staged_save = Some((d.edits, size)); Some(file) }
                            Err(err) => { drop(d); close_after_save = false; run.set_status(format!("Couldn't stage the workbook: {err}")); continue; }
                        }
                    } else { None };
                    let asked = e.borrow_mut().request("picker", token, PICKER_SLOT);
                    match asked.and_then(|id| {
                        use std::os::fd::AsFd;
                        let message = match (&run.target, what, commit_target) {
                            (Some(Target::Picked { commit, .. }), Asking::Versions, _) => ToChild::BrowseVersions(*commit),
                            (_, Asking::Recent, _) => ToChild::RecentWorkbooks,
                            (_, _, Some((token, _))) => ToChild::Commit(token),
                            _ => ToChild::Open(filter),
                        }.encode();
                        let result = match source {
                            Some(file) => e.borrow_mut().send_fd(id, message, file.as_fd()),
                            None => e.borrow_mut().send(id, message),
                        };
                        if result.is_err() { let _ = e.borrow_mut().detach_embed(id); }
                        result.map(|_| id)
                    }) {
                        Ok(id) => {
                            embed = Some(id);
                            asking = Some(what);
                            // A commit only awaits an acknowledgement. A picker slot here
                            // would intercept input to the workbook during the save.
                            run.sheets.ui.set(Ui { asking: what != Asking::Commit });
                        }
                        Err(err) => {
                            close_after_save = false;
                            staged_save = None;
                            if what == Asking::LinkedPicture { print_snap = None; }
                            run.set_status(format!("The picker didn't open: {err:?}"));
                        }
                    }
                }
                _ => { close_after_save = false; run.set_status("The system picker isn't available here."); }
            }
        }
        if let Some(e) = &embeds {
            let polled = e.borrow_mut().poll(Duration::ZERO);
            let ev = match polled {
                Ok(event) => event,
                Err(err) => {
                    eprintln!("atrium-sheets: lost Limen connection: {err:?}");
                    if staged_save.take().is_some() {
                        run.set_status("Lost contact with the picker before save confirmation.");
                    } else if asking.is_some() || print_snap.is_some() {
                        run.set_status("Lost contact with Limen. Please try again.");
                    }
                    // Embed IDs, queued requests and save acknowledgements belong
                    // to the old broker. Only a new user action may retry a save.
                    win.clear_slot_binder();
                    embeds = None;
                    next_limen_attempt = Instant::now();
                    asking = None;
                    embed = None;
                    close_after_save = false;
                    want_picker = None;
                    run.merge_base = None;
                    printing = None;
                    print_snap = None;
                    print_pdf = None;
                    run.sheets.ui.set(Ui { asking: false });
                    continue;
                }
            };
            let mut finish = false;
            let mut supplied_picture = false;
            let step = match (&mut printing, &ev) {
                (Some(p), Some(pev)) => p.handle(e, pev),
                _ => None,
            };
            let ours = step.is_some();
            match step {
                Some(Step::Relayout { paper, orientation, .. }) => {
                    if let Some(snap) = &print_snap {
                        let p = match orientation {
                            Orientation::Portrait => paper,
                            Orientation::Landscape => Paper { w_pt: paper.h_pt, h_pt: paper.w_pt },
                        };
                        print_pdf = Some((sheet_pdf(snap.clone(), p), true));
                    }
                }
                Some(Step::Done(o)) => {
                    printing = None;
                    print_snap = None;
                    print_pdf = None;
                    run.sheets.ui.set(Ui { asking: false });
                    run.set_status(match o {
                        Outcome::Queued => "Sent to the printer.".to_string(),
                        Outcome::Saved(name) => format!("Saved “{name}”."),
                        Outcome::Cancelled => String::new(),
                        Outcome::Failed(why) => format!("{why}."),
                    });
                }
                Some(Step::Pending) | None => {}
            }
            if !ours {
            match ev {
                Some(ParentEvent::MessageFd { embed: event, bytes }) => {
                    let fd = e.borrow_mut().take_fd();
                    if embed == Some(event) {
                    match (ToParent::decode(&bytes), fd) {
                        (Some(ToParent::File { .. }), Some(fd)) if asking == Some(Asking::LinkedPicture) => {
                            let result = print_snap.as_mut().and_then(Arc::get_mut)
                                .ok_or_else(|| "No linked picture is pending".to_string())
                                .and_then(|snap| snap.supply_link(fd.into()));
                            match result {
                                Ok(()) => supplied_picture = true,
                                Err(why) => run.set_status(format!("Couldn't print: {why}.")),
                            }
                            finish = true;
                        }
                        (Some(ToParent::Published { name, size, commit }), Some(_fd)) if asking == Some(Asking::SaveAs) => {
                            if let Some((edits, _)) = staged_save.filter(|(_, expected)| size == *expected) {
                                staged_save = None;
                                run.published(name, commit, edits);
                            } else { run.set_status("The picker returned an incomplete workbook."); }
                        }
                        (Some(ToParent::File { name, .. }), Some(fd))
                            if matches!(asking, Some(Asking::Compare | Asking::MergeBase | Asking::MergeIncoming)) =>
                            run.read_review(name, fd, asking.unwrap()),
                        _ => eprintln!("atrium-sheets: an odd message with a descriptor from the picker"),
                    }
                    }
                }
                Some(ParentEvent::Message { embed: event, bytes }) if embed == Some(event) => match ToParent::decode(&bytes) {
                    Some(ToParent::Committed) if asking == Some(Asking::Commit) => {
                        if let Some((edits, _)) = staged_save.take() {
                            let mut d = run.sheets.doc.borrow_mut();
                            d.status = if d.edits != edits {
                                "Saved. More recent edits are still pending.".into()
                            } else {
                                match d.saved() {
                                    Ok(()) => match run.target {
                                        Some(Target::Picked { format, .. }) => saved_status(&d, format),
                                        _ => "Saved.".into(),
                                    },
                                    Err(e) => format!("Saved, but autosave could not be updated: {e}"),
                                }
                            };
                        }
                        finish = true;
                    }
                    Some(ToParent::Cancelled) => {
                        if asking == Some(Asking::MergeIncoming) { run.merge_base = None; }
                        finish = true;
                    }
                    Some(ToParent::Done { .. }) => finish = true,
                    Some(ToParent::Failed { why }) => {
                        run.set_status(format!("The picker couldn't hand that over: {why}"));
                        finish = true;
                    }
                    _ => {}
                },
                Some(ParentEvent::ChildLost { embed: event }) if embed == Some(event) => {
                    if staged_save.is_some() { run.set_status("Scrinium disconnected before confirming the save."); }
                    finish = true;
                }
                _ => {}
            }
            }
            if finish {
                if let Some(id) = embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                if asking == Some(Asking::LinkedPicture) {
                    if supplied_picture {
                        if let Some(snap) = &print_snap {
                            if snap.next_link().is_some() {
                                want_picker = Some(Asking::LinkedPicture);
                            } else {
                                print_pdf = Some((sheet_pdf(snap.clone(), Paper::A4), false));
                            }
                        }
                    } else {
                        print_snap = None;
                        if run.sheets.doc.borrow().status == "Getting the sheet ready to print…" {
                            run.set_status("Printing cancelled before the linked picture was selected.");
                        }
                    }
                }
                asking = None;
                if staged_save.take().is_some() { close_after_save = false; }
                run.sheets.ui.set(Ui { asking: false });
                // Saved on the way out: close; cancelled: stay open.
                if std::mem::take(&mut close_after_save) && !run.sheets.doc.borrow().dirty {
                    break 'main;
                }
            }
        }
        if last_sync.elapsed() >= SYNC_EVERY {
            last_sync = Instant::now();
            let mut d = run.sheets.doc.borrow_mut();
            if let Some(a) = d.autosave() {
                if let Err(e) = a.sync() {
                    d.status = format!("Autosave failed: {e}");
                }
            }
        }
    }
    // Closing: a saved document leaves nothing behind; unsaved changes stay in the
    // autosave folder and come back at the next start.
    let d = std::mem::replace(&mut *run.sheets.doc.borrow_mut(), Doc::new(""));
    if d.dirty && !discard {
        let mut d = d;
        if let Some(a) = d.autosave() {
            let _ = a.sync();
        }
    } else {
        let _ = d.close();
    }
}

/// `--bench PATTERN ROWS SECS [HZ]`: the scroll gate on the wall clock.
fn bench_main(a: &[String]) -> i32 {
    let usage = || {
        eprintln!("usage: atrium-sheets --bench swipe|diagonal|wheel|page|jump|all ROWS SECS [HZ]");
        2
    };
    let (Some(p), Some(rows), Some(secs)) = (a.first(), a.get(1).and_then(|s| s.parse::<u32>().ok()), a.get(2).and_then(|s| s.parse::<f64>().ok())) else { return usage() };
    let hz: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let patterns: Vec<Pattern> = if p == "all" { Pattern::ALL.to_vec() } else { match Pattern::parse(p) { Some(p) => vec![p], None => return usage() } };
    let t0 = Instant::now();
    let d = bench::big_sheet(rows);
    eprintln!("atrium-sheets: bench sheet of {rows} rows built in {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let mut win = match Window::open(&WindowDesc::new("Sheets (bench)", SizeSpec::Fixed(1100, 720))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("atrium-sheets: frescod: {e}");
            return 1;
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let sheets = Sheets::new(
        d,
        Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        Mutable::with_dirty(GridState::default(), dirty.clone()),
        Mutable::with_dirty(Ui::default(), dirty.clone()),
        Mutable::with_dirty(0u64, dirty.clone()),
        (win.width, win.height),
    );
    let mut app = App::new_with_flag(sheets, dirty);
    let _ = win.paint(&mut app);
    let period = 1000.0 / hz;
    let mut all_pass = true;
    for p in patterns {
        let (w, h) = (win.width, win.height);
        let g = Sheets::grid_rect(w, h);
        let at = Point::new(g.x() + g.w() * 0.5, g.y() + g.h() * 0.5);
        let mut events = bench::script(p, secs, at).into_iter().peekable();
        let start = Instant::now();
        let mut frames = Vec::new();
        let mut parts: Vec<pergola::PaintTiming> = Vec::new();
        let mut moving = Vec::new();
        let n = (secs * hz) as usize;
        // One entry per vsync of wall-clock time: vsyncs that went by while a frame
        // was being made count as moving (the screen was behind), so a slow frame
        // shows up as missed vsyncs and, past 100 ms, as a stall.
        while moving.len() < n {
            let v = moving.len();
            let due = Duration::from_secs_f64(v as f64 * period / 1e3);
            if let Some(wait) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
            let behind = ((start.elapsed().as_secs_f64() * 1e3) / period) as usize;
            while moving.len() < behind.min(n) {
                moving.push(true);
            }
            if moving.len() >= n {
                break;
            }
            // Drain what frescod sent.
            while let Ok(Some(_)) = win.connection().wait_event(Some(Duration::from_micros(1))) {}
            let now = moving.len() as f64 * period;
            let mut input = false;
            while let Some((t, _)) = events.peek() {
                if *t > start.elapsed().as_secs_f64() * 1e3 {
                    break;
                }
                let (_, ev) = events.next().unwrap();
                app.handle_event(ev);
                input = true;
            }
            let wants = input || app.wants_frame() || app.is_dirty();
            moving.push(wants);
            if wants {
                let t = Instant::now();
                if let Err(e) = win.paint(&mut app) {
                    eprintln!("atrium-sheets: paint: {e}");
                    return 1;
                }
                frames.push(Frame { start: now, cost: t.elapsed().as_secs_f64() * 1e3 });
                parts.push(win.last_paint);
            }
        }
        let r = Report::new(&frames, period, &moving);
        all_pass &= r.passes(period);
        println!("atrium-sheets bench {} rows {rows} hz {hz}: {}", p.name(), r.line(period));
        // Where the time went: building the frame, or sending it (a write waits
        // while frescod is behind).
        let pct = |mut v: Vec<f64>, q: f64| {
            v.sort_by(f64::total_cmp);
            v.get(((v.len() as f64 - 1.0) * q).round().max(0.0) as usize).copied().unwrap_or(0.0)
        };
        let slow: Vec<&pergola::PaintTiming> = frames.iter().zip(&parts).filter(|(f, _)| f.cost > 2.0 * period).map(|(_, p)| p).collect();
        let mean = |f: &dyn Fn(&pergola::PaintTiming) -> f64| if slow.is_empty() { 0.0 } else { slow.iter().map(|p| f(p)).sum::<f64>() / slow.len() as f64 };
        println!(
            "  parts: build p50 {:.2} p99 {:.2} ms; send p50 {:.2} p99 {:.2} ms; deltas p50 {:.0} p99 {:.0}; {} slow frames (>2 periods): build {:.1} ms, send {:.1} ms, {:.0} deltas on average",
            pct(parts.iter().map(|p| p.build_ms).collect(), 0.5),
            pct(parts.iter().map(|p| p.build_ms).collect(), 0.99),
            pct(parts.iter().map(|p| p.send_ms).collect(), 0.5),
            pct(parts.iter().map(|p| p.send_ms).collect(), 0.99),
            pct(parts.iter().map(|p| p.deltas as f64).collect(), 0.5),
            pct(parts.iter().map(|p| p.deltas as f64).collect(), 0.99),
            slow.len(),
            mean(&|p| p.build_ms),
            mean(&|p| p.send_ms),
            mean(&|p| p.deltas as f64),
        );
        // Whether a slow send was this thread working, waiting on frescod, or waiting
        // for a CPU (preempted).
        println!(
            "  send of slow frames: cpu {:.1} ms, pop-ups {:.1} ms, preempted {:.1} times on average; worst send {:.1} ms with cpu {:.1} ms, {} preemptions",
            mean(&|p| p.send_cpu_ms),
            mean(&|p| p.popups_ms),
            mean(&|p| p.send_preempted as f64),
            slow.iter().map(|p| p.send_ms).fold(0.0, f64::max),
            slow.iter().max_by(|a, b| a.send_ms.total_cmp(&b.send_ms)).map_or(0.0, |p| p.send_cpu_ms),
            slow.iter().max_by(|a, b| a.send_ms.total_cmp(&b.send_ms)).map_or(0, |p| p.send_preempted),
        );
        // Let a fling settle, then back to the top for the next pattern.
        let settle = Instant::now();
        while app.wants_frame() && settle.elapsed() < Duration::from_secs(10) {
            let _ = win.paint(&mut app);
            std::thread::sleep(Duration::from_millis(8));
        }
        app.handle_event(pergola::Event::Key { kind: pergola::KeyEventKind::Down, key: pergola::Key::Home, modifiers: pergola::Modifiers { ctrl: true, ..Default::default() }, chars: String::new() });
        let _ = win.paint(&mut app);
    }
    if all_pass {
        0
    } else {
        3
    }
}

/// The sheet as PDF on `paper`, made on another thread (printing.md §5.1).
fn sheet_pdf(snap: Arc<atrium_sheets::print::Snapshot>, paper: Paper) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = tx.clone();
    let spawned = std::thread::Builder::new().name("pdf-print".into()).spawn(move || {
        let _ = t.send(atrium_sheets::print::pdf(&snap, paper.w_pt as f64, paper.h_pt as f64, ""));
    });
    if let Err(e) = spawned {
        let _ = tx.send(Err(e.to_string()));
    }
    rx
}

#[cfg(test)]
mod document_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn ods_save_reports_table_and_chart_downgrades() {
        use officina_grid::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, Series};
        use officina_grid::tables::Table;
        use officina_grid::{CellAddr, Rect};

        let mut doc = Doc::new("Sales.ods");
        let rect = Rect { sheet: 0, r0: 0, c0: 0, r1: 2, c1: 0 };
        doc.wb.set_input(CellAddr::new(0, 0, 0), "Units").unwrap();
        doc.wb.add_table(Table {
            name: "Sales".into(), rect, header: true, totals: true,
            columns: vec!["Units".into()], style: Some("TableStyleMedium2".into()), stripes: true,
        }).unwrap();
        doc.wb.define_name("Rate", None, "=3").unwrap();
        doc.wb.set_input(CellAddr::new(0, 0, 1), "=Rate").unwrap();
        doc.wb.add_chart(Chart {
            kind: ChartKind::Column, grouping: Grouping::Standard, title: None,
            series: vec![Series {
                name: Some(ChartText::Lit("Units shipped".into())), cats: None, vals: rect, kind: None,
            }],
            legend: None, x_title: None, y_title: None,
            anchor: Anchor { sheet: 0, from: (4, 2, 0.0, 0.0), to: (12, 8, 0.0, 0.0) },
        }).unwrap();

        assert!(!saved_status(&doc, Format::Ods).contains("converted table references"));
        doc.wb.set_input(CellAddr::new(0, 0, 1), "=SUM(Sales[Units])").unwrap();
        assert_eq!(
            saved_status(&doc, Format::Ods),
            "Saved “Sales.ods”. ODS converted table references to cell ranges, omitted table styles, kept table totals in plain cells, omitted typed chart series names."
        );
        assert_eq!(saved_status(&doc, Format::Xlsx), "Saved “Sales.ods”.");
    }

    #[test]
    fn delimited_save_reports_formula_results_in_the_written_format() {
        for (name, input, label) in [
            ("Data.csv", b"a,b\n7,0\n".as_slice(), "CSV"),
            ("Data.TSV", b"a,b,c,d\tCalc\n7\t0\nsolo\n".as_slice(), "TSV"),
        ] {
            let (mut doc, _) = Doc::from_csv(name, input).unwrap();
            doc.type_into(1, 1, "=A2*3");
            let bytes = encode(&doc, Format::Csv).unwrap();
            let (back, _) = Doc::from_csv(name, &bytes).unwrap();
            let result = officina_grid::CellAddr::new(0, 1, 1);
            assert_eq!(back.wb.get(result), officina_grid::Value::Num(21.0));
            assert!(back.wb.formula_text(result).is_none());
            assert_eq!(
                saved_status(&doc, Format::Csv),
                format!("Saved “{name}”. {label} keeps formula results as values.")
            );
        }
    }

    #[test]
    fn ods_save_reports_fixed_spill_references() {
        let mut doc = Doc::new("Arrays.ods");
        doc.type_into(0, 0, "=SEQUENCE(2)");
        assert_eq!(saved_status(&doc, Format::Ods), "Saved “Arrays.ods”.");
        doc.type_into(0, 1, "=SUM(A1#)");
        assert_eq!(
            saved_status(&doc, Format::Ods),
            "Saved “Arrays.ods”. ODS replaced spill references with fixed ranges or reference errors."
        );
        assert_eq!(saved_status(&doc, Format::Xlsx), "Saved “Arrays.ods”.");
    }

    #[test]
    fn ods_save_reports_flattening_comment_threads() {
        use officina_grid::comments::{Comment, ThreadMessage, ThreadPerson};
        let mut doc = Doc::new("Notes.ods");
        doc.wb.set_comments(0, vec![(0, 0, Some(Comment::new("Ana", "Plain note")))]);
        assert_eq!(saved_status(&doc, Format::Ods), "Saved “Notes.ods”.");
        let thread = Comment::threaded(vec![ThreadMessage {
            id: 1, parent: None,
            author: Arc::new(ThreadPerson { id: 2, name: "Ana".into(), user_id: "".into(), provider_id: "".into() }),
            timestamp: "".into(), done: false, text: "Thread".into(), mentions: Arc::default(),
        }]).unwrap();
        doc.wb.set_comments(0, vec![(0, 0, Some(thread))]);
        assert_eq!(saved_status(&doc, Format::Ods), "Saved “Notes.ods”. ODS saved comment threads as plain notes.");
        assert_eq!(saved_status(&doc, Format::Xlsx), "Saved “Notes.ods”.");
    }

    // Run under jail(8) with a workbook descriptor queued on stdin.
    #[test]
    #[ignore = "requires a workbook launch descriptor on stdin"]
    fn workbook_jail_descriptor() {
        let received = receive_workbook().unwrap().unwrap();
        assert!(received.write);
        let mut file = received.file;
        let bytes = read_all(&mut file).unwrap();
        let (before, after) = match received.name.as_str() {
            "first.csv" => (1.0, 11.0),
            "second.csv" => (2.0, 22.0),
            name => panic!("unexpected workbook {name}"),
        };
        let mut doc = Doc::from_csv(&received.name, &bytes).unwrap().0;
        match doc.wb.value(0, 0, 0) {
            officina_grid::Value::Num(n) if n == before => {
                assert!(doc.type_into(0, 0, &after.to_string()));
                write_over(&mut file, &encode(&doc, Format::Csv).unwrap()).unwrap();
                eprintln!("saved {}", received.name);
            }
            value => {
                assert_eq!(value, officina_grid::Value::Num(after));
                eprintln!("reopened {}", received.name);
            }
        }
    }

    #[test]
    fn workbook_open_carries_the_scrinium_commit_handle() {
        let (tx, rx) = UnixStream::pair().unwrap();
        let path = std::env::temp_dir().join(format!("sheets-grant-{}.osheet", std::process::id()));
        fs::write(&path, Doc::new("Budget").stored_native().unwrap()).unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        atrium_open::send_with_commit(&tx, "Budget.osheet", "application/vnd.atrium.osheet", true,
            file.as_raw_fd(), Some([42;16])).unwrap();
        let opened = atrium_open::Channel::from_stream(rx).recv(Some(Duration::from_secs(2))).unwrap().unwrap();
        assert_eq!(opened.commit, Some([42;16]));
        assert_eq!(opened.name, "Budget.osheet");
        let mut file = opened.file;
        Doc::from_native(&opened.name, &read_all(&mut file).unwrap()).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn workbook_launch_descriptors_save_and_reopen_independently() {
        let root = new_autosave_dir(&std::env::temp_dir());
        fs::create_dir(&root).unwrap();
        let paths = [root.join("one.osheet"), root.join("two.osheet")];
        fn connect(path: &Path) -> atrium_open::Channel {
            let file = OpenOptions::new().read(true).write(true).open(path).unwrap();
            let (send, recv) = UnixStream::pair().unwrap();
            atrium_open::send(
                &send, path.file_name().unwrap().to_str().unwrap(),
                "application/vnd.atrium.osheet", true, file.as_raw_fd(),
            ).unwrap();
            atrium_open::Channel::from_stream(recv)
        }

        for (path, start) in paths.iter().zip(["11", "22"]) {
            let mut doc = Doc::new("Workbook");
            assert!(doc.type_into(0, 0, start));
            fs::write(path, encode(&doc, Format::Native).unwrap()).unwrap();
        }

        let channels = paths.each_ref().map(|path| connect(path));
        for ((mut channel, before), after) in channels.into_iter()
            .zip(["11", "22"])
            .zip(["111", "222"])
        {
            let received = channel.recv(Some(Duration::ZERO)).unwrap().unwrap();
            assert!(received.write);
            let mut file = received.file;
            let mut doc = Doc::from_native(&received.name, &read_all(&mut file).unwrap()).unwrap();
            assert_eq!(doc.wb.value(0, 0, 0), officina_grid::Value::Num(before.parse().unwrap()));
            assert!(doc.type_into(0, 0, after));
            write_over(&mut file, &encode(&doc, Format::Native).unwrap()).unwrap();
            assert_eq!(channel.recv(Some(Duration::ZERO)).err().unwrap().kind(), io::ErrorKind::UnexpectedEof);
        }

        for (path, expected) in paths.iter().zip(["111", "222"]) {
            let received = connect(path).recv(Some(Duration::ZERO)).unwrap().unwrap();
            let doc = Doc::from_native(&received.name, &read_all(&mut { received.file }).unwrap()).unwrap();
            assert_eq!(doc.wb.value(0, 0, 0), officina_grid::Value::Num(expected.parse().unwrap()));
        }
        fs::remove_dir_all(root).unwrap();
    }
}
