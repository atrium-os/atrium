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
//! writes through the granted descriptor. Save As asks Scrinium for a new
//! native workbook descriptor, which becomes the target for subsequent saves.
//! FRESCO_SOCKET and LIMEN_SOCKET as usual; without limend the app runs, and Open and
//! Save As say the picker is not available.

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
    /// A picked file, open read-write.
    Picked { file: File, format: Format },
    /// A path supplied for development.
    Path { path: PathBuf, format: Format },
}

/// What the picker is up for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asking {
    Open,
    SaveAs,
    /// The copy to compare with.
    Compare,
    MergeBase,
    MergeIncoming,
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

/// Writes the document over a picked file. The autosave folder still holds the
/// document until this returns, so a crash halfway loses nothing.
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

fn new_workbook(activation: Option<fresco_protocol::activation::Token>) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION, WORKERS_SOCKET_PATH};
    use std::os::unix::net::UnixStream;
    let activation = activation.ok_or("Choose New in the Sheets window")?;
    let mut socket = UnixStream::connect(WORKERS_SOCKET_PATH).map_err(|e| e.to_string())?;
    let timeout = Some(Duration::from_secs(2));
    socket.set_read_timeout(timeout).and_then(|_| socket.set_write_timeout(timeout)).map_err(|e| e.to_string())?;
    match round_trip(&mut socket, &Request::Hello { version: PROTO_VERSION }).map_err(|e| e.to_string())? {
        Response::Hello { .. } => {}
        response => return Err(format!("The workbook broker answered {response:?}")),
    }
    match round_trip(&mut socket, &Request::NewWorkbook { activation }).map_err(|e| e.to_string())? {
        Response::Opened { .. } => Ok(()),
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
        let Some(t) = self.target.as_mut() else { return false };
        let format = match t {
            Target::Picked { format, .. } | Target::Path { format, .. } => *format,
        };
        if format != Format::Native && self.sheets.doc.borrow().wb.tracked_cells().is_some() {
            self.set_status("Choose where to save an .osheet copy with its tracked cell changes.");
            return false;
        }
        // A file keeps every value as it should be: finish recalculating first.
        { let mut d = self.sheets.doc.borrow_mut(); if d.wb.recalc_pending() { d.wb.recalc(); } }
        let r = {
            let d = self.sheets.doc.borrow();
            encode(&d, format).and_then(|bytes| match t {
                Target::Picked { file, .. } => write_over(file, &bytes),
                Target::Path { path, .. } => {
                    let mut f = OpenOptions::new().write(true).open(&*path)?;
                    write_over(&mut f, &bytes)
                }
            })
        };
        let mut d = self.sheets.doc.borrow_mut();
        match r.and_then(|_| d.saved()) {
            Ok(()) => {
                let lost = format == Format::Csv && d.wb.groups().next().is_some();
                // Macros go back only into their own kind of file (§5.3).
                let fam = d.wb.macros().map(|m| m.family);
                let macros_lost = match (fam, format) {
                    (None, _) => false,
                    (Some(officina_grid::macros::MacroFamily::Ooxml), Format::Xlsm) | (Some(officina_grid::macros::MacroFamily::Odf), Format::Ods) | (Some(_), Format::Native) => false,
                    _ => true,
                };
                d.status = if lost {
                    format!("Saved “{}” as CSV: values only, formulas are not kept in CSV.", d.name)
                } else if macros_lost {
                    format!("Saved “{}”. Its macros aren't in this file: this format can't hold them.", d.name)
                } else {
                    format!("Saved “{}”.", d.name)
                };
            }
            Err(e) => d.status = format!("Couldn't save “{}”: {e}. Your changes are still here and in the autosave.", d.name),
        }
        drop(d);
        self.sheets.changed();
        true
    }

    fn save_as(&mut self, name: String, mut file: File) {
        let result = {
            let mut doc = self.sheets.doc.borrow_mut();
            if doc.wb.recalc_pending() { doc.wb.recalc(); }
            encode(&doc, Format::Native).and_then(|bytes| write_over(&mut file, &bytes))
        };
        match result {
            Ok(()) => {
                self.target = Some(Target::Picked { file, format: Format::Native });
                let mut doc = self.sheets.doc.borrow_mut();
                doc.name = name.clone();
                doc.status = match doc.saved() {
                    Ok(()) => format!("Saved as “{name}”."),
                    Err(e) => format!("Saved as “{name}”, but couldn't clear the autosave: {e}."),
                };
                drop(doc);
                self.sheets.changed();
            }
            Err(e) => self.set_status(format!("Couldn't save “{name}”: {e}.")),
        }
    }

    /// A picked file arrived.
    fn opened(&mut self, name: String, write: bool, fd: std::os::fd::OwnedFd) {
        let mut f = File::from(fd);
        let format = format_of(&name);
        match read_all(&mut f) {
            Ok(data) => self.open_bytes(name, data, write.then_some(Target::Picked { file: f, format }), !write, None),
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
                    d.status = format!("{} It opened read-only: Save asks where to put a copy.", d.status);
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
    let dedicated = workbook.is_some() || args.get(1).is_some_and(|arg| arg == "--blank-document");
    officina_convert::pictures::install();
    let root = state_root();
    // The custom lists fills follow, kept beside the autosaves.
    if let Some(dir) = root.parent() {
        atrium_sheets::lists::init(dir);
    }
    let _ = fs::create_dir_all(&root);
    // A document a crash left comes back first; it, or a file named on the
    // command line, is read on its own thread once the window shows.
    let mut locked: Option<(PathBuf, String)> = None;
    let mut restore: Option<(PathBuf, String)> = None;
    let mut start_file: Option<(String, Vec<u8>, Target)> = None;
    if let Some((dir, name)) = (!dedicated).then(|| doc::pending(&root).pop()).flatten() {
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
    let embeds = match Embeds::connect() {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("atrium-sheets: no limend ({e:?}); the picker is not available");
            None
        }
    };
    if let Some(e) = &embeds {
        win.set_slot_binder(Embeds::binder(e));
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let mut sheets = Sheets::new(
        Doc::new(""),
        Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        Mutable::with_dirty(GridState::default(), dirty.clone()),
        Mutable::with_dirty(Ui::default(), dirty.clone()),
        Mutable::with_dirty(0u64, dirty.clone()),
        (win.width, win.height),
    );
    sheets.launcher = launcher;
    let mut run = Run { sheets, target: None, locked: None, root: root.clone(), opening: None, merge_base: None };
    if !launcher {
        run.show(doc, target);
    }
    if let Some(file) = workbook {
        eprintln!("atrium-sheets: “{}” opened in its document jail{}", file.name,
                  if file.write { " (write)" } else { "" });
        run.opened(file.name, file.write, file.file.into());
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
    // Closing waits on Save As (true), and Don't Save drops the changes.
    let mut close_after_save = false;
    let mut discard = false;
    let mut recalculating = false;
    // Printing: the sheet as it was asked for, the PDF being made (true once
    // the print sheet is up: the answer to its Relayout), and the sheet.
    let mut print_snap: Option<Arc<atrium_sheets::print::Snapshot>> = None;
    let mut print_pdf: Option<(std::sync::mpsc::Receiver<Result<Vec<u8>, String>>, bool)> = None;
    let mut printing: Option<Printing> = None;
    let mut want_print = false;
    'main: loop {
        // While a recalculation is unfinished, a slice of it each turn (the
        // window answers in between) and no waiting for input.
        let opening = run.poll_open(&mut want_picker);
        let wait = if recalculating { 1 } else if asking.is_some() || opening { 20 } else { 250 };
        let wait = if printing.is_some() || print_pdf.is_some() { wait.min(20) } else { wait };
        // Ctrl+P prints, as the toolbar's Print… does.
        let stepped = win.step_with(&mut app, Some(Duration::from_millis(wait)), |ev| match ev {
            fresco_client::Event::Key { hid_usage: 0x13, pressed: true, modifiers, repeat: false, .. } if modifiers & 0b0001_0001 != 0 => {
                want_print = true;
                true
            }
            _ => false,
        });
        match stepped {
            // Unsaved changes: ask first. Asked to close again while asking (or
            // while picking), the changes stay in the autosave folder.
            Ok(StepResult::Closed) if run.sheets.doc.borrow().dirty && !run.sheets.closing.get() && asking.is_none() => {
                run.sheets.closing.set(true);
                run.sheets.changed();
            }
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        {
            let mut d = run.sheets.doc.borrow_mut();
            d.recalc_budget = Some(atrium_sheets::doc::RECALC_SLICE);
            let was = recalculating;
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
                Cmd::NewWorkbook(activation) => {
                    if let Err(why) = new_workbook(activation) {
                        run.set_status(format!("Couldn't start a workbook: {why}"));
                    }
                }
                Cmd::Open => want_picker = Some(Asking::Open),
                Cmd::Data(atrium_sheets::data::DataCmd::CompareWith) => want_picker = Some(Asking::Compare),
                Cmd::Data(atrium_sheets::data::DataCmd::MergeCopies) => {
                    run.merge_base = None;
                    want_picker = Some(Asking::MergeBase);
                }
                Cmd::PasswordDone => run.password_done(),
                Cmd::PasswordClose => run.password_cancelled(),
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::Print => want_print = true,
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
                        print_pdf = Some((sheet_pdf(snap.clone(), Paper::A4), false));
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
                        Asking::SaveAs => Filter::save(&save_name(&name), ""),
                    };
                    let asked = e.borrow_mut().request("picker", token, PICKER_SLOT);
                    match asked.and_then(|id| e.borrow_mut().send(id, ToChild::Open(filter).encode()).map(|_| id)) {
                        Ok(id) => {
                            embed = Some(id);
                            asking = Some(what);
                            run.sheets.ui.set(Ui { asking: true });
                        }
                        Err(err) => run.set_status(format!("The picker didn't open: {err:?}")),
                    }
                }
                _ => run.set_status("The system picker isn't available here."),
            }
        }
        if let Some(e) = &embeds {
            let ev = e.borrow_mut().poll(Duration::from_millis(0));
            let mut finish = false;
            let step = match (&mut printing, &ev) {
                (Some(p), Ok(Some(pev))) => p.handle(e, pev),
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
                Ok(Some(ParentEvent::MessageFd { bytes, .. })) => {
                    let fd = e.borrow_mut().take_fd();
                    match (ToParent::decode(&bytes), fd) {
                        (Some(ToParent::File { name, write: true, .. }), Some(fd)) if asking == Some(Asking::SaveAs) => run.save_as(name, fd.into()),
                        (Some(ToParent::File { name, .. }), Some(fd))
                            if matches!(asking, Some(Asking::Compare | Asking::MergeBase | Asking::MergeIncoming)) =>
                            run.read_review(name, fd, asking.unwrap()),
                        _ => eprintln!("atrium-sheets: an odd message with a descriptor from the picker"),
                    }
                }
                Ok(Some(ParentEvent::Message { bytes, .. })) => match ToParent::decode(&bytes) {
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
                Ok(Some(ParentEvent::ChildLost { .. })) => finish = true,
                Ok(_) => {}
                Err(err) => {
                    eprintln!("atrium-sheets: limend: {err:?}");
                    finish = asking.is_some();
                }
            }
            }
            if finish {
                if let Some(id) = embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                asking = None;
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
