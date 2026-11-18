//! atrium-write: the word processor (docs/spec/officina.md §7, O3c).
//!
//!   atrium-write          a new document (or the one a crash left, restored)
//!   atrium-write FILE     open FILE by path (development; apps get files from
//!                         the picker, or from Scrinium on the document
//!                         channel, opening-files.md §6.2)
//!
//! Files come from the system picker (scrinium.md §4.1): Open asks for one
//! file, read-write; Save writes back to it. Save As asks for a folder with
//! write access and writes `NAME.owrite` there (temp file, then rename), until
//! the picker's own Save As sheet exists (§4.2). Every change is journaled to a
//! private autosave folder (`$XDG_STATE_HOME` or `~/.local/state`,
//! `atrium-write/autosave/`); a document that did not close cleanly is
//! restored at the next start. `.owrite` is the native file; anything else
//! opens and saves as plain text (`.docx` and `.odt` come with O3e).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_write::app::{Cmd, Ui, Write, PICKER_SLOT};
use atrium_write::doc::{self, Doc};
use atrium_write::files::{encode, format_of, load, load_with, stem_of, Format};
use officina_convert::launch::ConvertError;
use limen::picker::{Filter, Kind, ToChild, ToParent};
use limen::proto::ParentEvent;
use pergola::embed::Embeds;
use pergola::widgets::ScrollState;
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};

/// The largest file Open reads.
const MAX_FILE: u64 = 2 << 30;
/// How often the journal is made durable.
const SYNC_EVERY: Duration = Duration::from_secs(2);

/// Where the user's file is, for Save.
enum Target {
    Picked { file: File, format: Format },
    Path { path: PathBuf, format: Format },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Asking {
    Open,
    Compare,
    Merge,
    SaveAs,
    Picture,
    ExportPdf,
}

fn state_root() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("atrium-write/autosave")
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

/// Writes the document over a picked file. The autosave folder still holds the
/// document until this returns, so a crash halfway loses nothing.
fn write_over(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Writes `bytes` to `dir/name` through a temp file and a rename; never replaces a
/// file already there (`name 2.owrite` and so on instead). Returns the path.
fn write_new(dir: &Path, stem: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    place_new(dir, stem, "owrite", |f| f.write_all(bytes))
}

/// A new file `STEM.EXT` (or `STEM N.EXT`) in `dir`, written by `fill` to a
/// hidden temporary first, so nothing half-written or existing is replaced.
fn place_new(dir: &Path, stem: &str, ext: &str, fill: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<PathBuf> {
    let stem: String = stem.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let stem = stem.trim_start_matches('.');
    let stem = if stem.is_empty() { "Document" } else { stem };
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        if let Err(e) = fill(&mut f).and_then(|_| f.sync_all()) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    for n in 1..1000 {
        let name = if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} {n}.{ext}") };
        let path = dir.join(&name);
        // A hard link fails when the name is taken, so nothing is replaced.
        match fs::hard_link(&tmp, &path) {
            Ok(()) => {
                let _ = fs::remove_file(&tmp);
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "every name is taken"))
}


/// A protected file waiting for its password.
enum Locked {
    File { name: String, data: Vec<u8>, target: Option<Target> },
    /// A protected document's autosave folder, to restore.
    Recovery { dir: PathBuf, name: String },
}

struct Run {
    write: Write,
    target: Option<Target>,
    locked: Option<Locked>,
    root: PathBuf,
    /// A PDF export running in the background: its latest word.
    export: Option<std::sync::Arc<std::sync::Mutex<(String, bool)>>>,
}

impl Run {
    fn set_status(&self, s: impl Into<String>) {
        self.write.doc.borrow_mut().status = s.into();
        self.write.changed();
    }

    /// Puts `d` on screen with its own autosave folder (a restored one keeps its own). The document it replaces
    /// closes cleanly if saved; with unsaved changes its autosave folder stays, and
    /// it comes back at the next start.
    fn show(&mut self, mut d: Doc, mut target: Option<Target>) {
        // Write reads Word 97–2003 files but doesn't write them: Save asks where to keep a copy.
        if matches!(target, Some(Target::Picked { format: Format::Doc, .. } | Target::Path { format: Format::Doc, .. })) {
            d.status = format!("{} Write reads Word 97–2003 files but doesn't write them: Save asks where to keep a copy.", d.status);
            target = None;
        }
        if d.autosave().is_none() {
            if let Err(e) = d.autosave_to(&new_autosave_dir(&self.root)) {
                d.status = format!("{} Autosave is off: {e}.", d.status);
            }
        }
        let old = self.write.replace(d);
        self.target = target;
        if old.dirty {
            let name = old.name.clone();
            let mut old = old;
            if let Some(a) = old.autosave() {
                let _ = a.sync();
            }
            drop(old);
            let s = self.write.doc.borrow().status.clone();
            self.set_status(format!("{s} Unsaved changes to “{name}” are kept and come back next time Write starts."));
        } else {
            let _ = old.close();
        }
    }

    fn save(&mut self) -> bool {
        let Some(t) = self.target.as_mut() else { return false };
        let format = match t {
            Target::Picked { format, .. } | Target::Path { format, .. } => *format,
        };
        let r = {
            let d = self.write.doc.borrow();
            Ok::<_, io::Error>(encode(&d, format)).and_then(|bytes| match t {
                Target::Picked { file, .. } => write_over(file, &bytes),
                Target::Path { path, .. } => {
                    let mut f = OpenOptions::new().write(true).open(&*path)?;
                    write_over(&mut f, &bytes)
                }
            })
        };
        let mut d = self.write.doc.borrow_mut();
        match r.and_then(|_| d.saved()) {
            Ok(()) => {
                let open = d.ed.borrow().doc.conflicts().len();
                d.status = if format == Format::Text {
                    format!("Saved “{}” as plain text: formatting is not kept in a text file.", d.name)
                } else if open > 0 && format != Format::Native {
                    format!("Saved “{}” with this copy's version of {open} unsettled conflict{}: settle them first to keep the other version in this format.", d.name, if open == 1 { "" } else { "s" })
                } else {
                    format!("Saved “{}”.", d.name)
                };
            }
            Err(e) => d.status = format!("Couldn't save “{}”: {e}. Your changes are still here and in the autosave.", d.name),
        }
        drop(d);
        self.write.changed();
        true
    }

    fn save_into(&mut self, dir: &Path) {
        let (stem, bytes) = {
            let d = self.write.doc.borrow();
            (stem_of(&d.name).to_string(), Ok::<_, io::Error>(encode(&d, Format::Native)))
        };
        let r = bytes.and_then(|b| write_new(dir, &stem, &b));
        match r {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                {
                    let mut d = self.write.doc.borrow_mut();
                    d.name = name.clone();
                    let _ = d.saved();
                    d.status = format!("Saved as “{name}”.");
                }
                self.target = Some(Target::Path { path, format: Format::Native });
                self.write.changed();
            }
            Err(e) => self.set_status(format!("Couldn't save there: {e}.")),
        }
    }

    /// Exports the document as it is now to `STEM.pdf` in `dir`, in the
    /// background; the status line follows it.
    fn export_pdf(&mut self, dir: &Path) {
        if self.export.is_some() {
            self.set_status("A PDF export is already running.");
            return;
        }
        let (doc, stem, title) = {
            let d = self.write.doc.borrow();
            let e = d.ed.borrow();
            (e.doc.clone(), stem_of(&d.name).to_string(), stem_of(&d.name).to_string())
        };
        let word = std::sync::Arc::new(std::sync::Mutex::new((format!("Exporting “{stem}.pdf”…"), false)));
        self.export = Some(word.clone());
        let dir = dir.to_path_buf();
        let spawned = std::thread::Builder::new().name("pdf-export".into()).spawn(move || {
            let r = officina_text::layout::PinnedFonts::new().map_err(|e| io::Error::other(e)).and_then(|fonts| {
                let mut stats = None;
                let path = place_new(&dir, &stem, "pdf", |f| {
                    let w = io::BufWriter::new(f);
                    let mut tick = Instant::now();
                    let (w, s) = officina_pdf::export(&doc, &fonts, &fonts, &officina_pdf::Info { title }, w, &mut |done, total| {
                        if tick.elapsed() >= Duration::from_millis(250) {
                            tick = Instant::now();
                            word.lock().unwrap().0 = format!("Exporting PDF: page {done} of {total}…");
                        }
                    })?;
                    w.into_inner().map_err(|e| e.into_error())?;
                    stats = Some(s);
                    Ok(())
                })?;
                Ok((path, stats.unwrap_or_default()))
            });
            *word.lock().unwrap() = match r {
                Ok((path, s)) => (format!("Exported “{}” ({} pages).", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), s.pages), true),
                Err(e) => (format!("Couldn't export the PDF: {e}."), true),
            };
        });
        if let Err(e) = spawned {
            self.export = None;
            self.set_status(format!("Couldn't start the export: {e}."));
        }
    }

    /// Shows how the export is going; forgets it once it ends.
    fn poll_export(&mut self) {
        let Some(w) = &self.export else { return };
        let (msg, done) = w.lock().unwrap().clone();
        if self.write.doc.borrow().status != msg {
            self.set_status(msg);
        }
        if done {
            self.export = None;
        }
    }

    /// A picked file arrived.
    fn opened(&mut self, name: String, write: bool, fd: std::os::fd::OwnedFd) {
        let mut f = File::from(fd);
        let format = format_of(&name);
        match read_all(&mut f) {
            Ok(data) => {
                let read_only = !write;
                self.open_bytes(name, data, write.then_some(Target::Picked { file: f, format }), read_only, None);
            }
            Err(e) => self.set_status(format!("Couldn't read “{name}”: {e}")),
        }
    }

    /// Opens `data` (named `name`), with `password` when it is protected;
    /// without the right one, the password bar asks and the file waits.
    fn open_bytes(&mut self, name: String, data: Vec<u8>, target: Option<Target>, read_only: bool, password: Option<&str>) {
        match load_with(&name, &data, password) {
            Ok(mut d) => {
                if read_only {
                    d.status = format!("{} It opened read-only: Save asks where to put a copy.", d.status);
                }
                self.show(d, target);
            }
            Err(e @ (ConvertError::NeedsPassword | ConvertError::WrongPassword)) => {
                let again = e == ConvertError::WrongPassword;
                self.write.ask_password(&if again { format!("Wrong password for “{name}”. Try again") } else { format!("“{name}” is protected. Its password") }, false);
                self.locked = Some(Locked::File { name, data, target });
            }
            Err(ConvertError::Failed(e)) => self.set_status(e),
        }
    }

    /// Restores a protected document's autosave folder, asking for its
    /// password.
    fn recover_locked(&mut self, dir: PathBuf, name: String, password: Option<&str>) {
        match password.map(|pw| doc::recover_with(&dir, Some(pw))) {
            Some(Ok((mut d, rec))) => {
                d.status = format!("Restored “{name}” as it was before Write last stopped ({} changes since its last save).", rec.actions);
                self.show(d, None);
            }
            Some(Err(e)) if e.kind() != io::ErrorKind::PermissionDenied => self.set_status(format!("Couldn't restore “{name}”: {e}")),
            r => {
                let again = r.is_some();
                self.write.ask_password(&if again { format!("Wrong password for “{name}”. Try again") } else { format!("Restore “{name}”: its password") }, false);
                self.locked = Some(Locked::Recovery { dir, name });
            }
        }
    }

    /// The password bar's answer: a waiting file opened, or the document's
    /// password set.
    fn password_done(&mut self) {
        let (bar, text) = self.write.take_password();
        if bar.setting {
            let pw = (!text.is_empty()).then_some(text.as_str());
            let r = self.write.doc.borrow_mut().set_password(pw);
            let mut d = self.write.doc.borrow_mut();
            d.dirty = true;
            d.status = match (r, pw) {
                (Err(e), _) => format!("The password is set, but autosave stopped: {e}."),
                (Ok(()), Some(_)) => format!("“{}” is protected with a password from its next save. Plain text files can't carry one.", d.name),
                (Ok(()), None) => format!("“{}” has no password from its next save.", d.name),
            };
            drop(d);
            self.write.changed();
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
            Some(Locked::Recovery { name, .. }) => self.set_status(format!("“{name}” wasn't restored; it comes back next time Write starts.")),
            None => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Pictures are decoded in the convert worker, never here.
    officina_convert::pictures::install();

    let root = state_root();
    let _ = fs::create_dir_all(&root);
    // A document a crash left comes back first.
    let mut first: Option<(Doc, Option<Target>)> = None;
    let mut locked: Option<(PathBuf, String)> = None;
    let mut locked_file: Option<(String, Vec<u8>, Target)> = None;
    if let Some((dir, name)) = doc::pending(&root).pop() {
        if doc::recovery_locked(&dir) {
            locked = Some((dir, name));
        } else {
            match doc::recover(&dir) {
                Ok((mut d, rec)) => {
                    d.status = format!("Restored “{name}” as it was before Write last stopped ({} changes since its last save).", rec.actions);
                    first = Some((d, None));
                }
                Err(e) => eprintln!("atrium-write: couldn't restore {}: {e}", dir.display()),
            }
        }
    }
    if first.is_none() && locked.is_none() {
        if let Some(path) = args.get(1) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
            match fs::read(&p) {
                Ok(data) => match load_with(&name, &data, None) {
                    Ok(d) => first = Some((d, Some(Target::Path { path: p, format: format_of(&name) }))),
                    Err(ConvertError::Failed(e)) => eprintln!("atrium-write: {e}"),
                    Err(_) => locked_file = Some((name, data, Target::Path { path: p.clone(), format: format_of(&p.to_string_lossy()) })),
                },
                Err(e) => eprintln!("atrium-write: {e}"),
            }
        }
    }
    let (doc, target) = first.unwrap_or_else(|| (Doc::new("Untitled"), None));

    let mut win = match Window::open(&WindowDesc::new("Write", SizeSpec::Fixed(1180, 860))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("atrium-write: frescod: {e}");
            std::process::exit(1);
        }
    };
    let embeds = match Embeds::connect() {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("atrium-write: no limend ({e:?}); the picker is not available");
            None
        }
    };
    if let Some(e) = &embeds {
        win.set_slot_binder(Embeds::binder(e));
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let write = Write::new(
        Doc::new(""),
        Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        Mutable::with_dirty(Ui::default(), dirty.clone()),
        Mutable::with_dirty(0u64, dirty.clone()),
        (win.width, win.height),
    );
    write.keep_words(root.join("words"));
    let mut run = Run { write, target: None, locked: None, root: root.clone(), export: None };
    run.show(doc, target);
    if let Some((dir, name)) = locked {
        run.recover_locked(dir, name, None);
    } else if let Some((name, data, t)) = locked_file {
        run.open_bytes(name, data, Some(t), false, None);
    }
    // For screenshots: start with a bar open (ATRIUM_WRITE_BAR=spell, comment, xref:N, or find:TEXT).
    match std::env::var("ATRIUM_WRITE_BAR").ok().as_deref() {
        Some("spell") => run.write.cmds.lock().unwrap().push(Cmd::SpellOpen),
        Some(f) if f.starts_with("review") => {
            // BAR=review:N opens review on edit N (from 0).
            let n: usize = f.strip_prefix("review:").and_then(|n| n.parse().ok()).unwrap_or(0);
            let mut q = run.write.cmds.lock().unwrap();
            q.push(Cmd::ReviewOpen);
            q.extend(std::iter::repeat_n(Cmd::ReviewNext, n));
        }
        Some(f) if f.starts_with("merge:") => {
            // BAR=merge:PATH merges another reviewed copy at PATH into the document.
            let p = std::path::Path::new(&f[6..]);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match fs::read(p).map_err(|e| e.to_string()).and_then(|data| load(&name, &data)) {
                Ok(other) => run.write.merge_with(&other, &name),
                Err(e) => run.set_status(e),
            }
        }
        Some(f) if f.starts_with("compare:") => {
            // BAR=compare:PATH compares an earlier version at PATH with the document.
            let p = std::path::Path::new(&f[8..]);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match fs::read(p).map_err(|e| e.to_string()).and_then(|data| load(&name, &data)) {
                Ok(older) => run.write.compare_with(&older, &name),
                Err(e) => run.set_status(e),
            }
        }
        Some("comment") => run.write.cmds.lock().unwrap().push(Cmd::CommentOpen(0)),
        // BAR=xref:N opens the cross-reference bar on heading N (from 0).
        Some(f) if f.starts_with("xref") => {
            let n: usize = f.strip_prefix("xref:").and_then(|n| n.parse().ok()).unwrap_or(0);
            let mut q = run.write.cmds.lock().unwrap();
            q.push(Cmd::XrefOpen);
            q.extend(std::iter::repeat_n(Cmd::XrefMove(true), n));
        }
        Some(f) if f.starts_with("find:") => {
            run.write.find_text.set(pergola::text_edit::TextEdit::new(&f[5..]));
            run.write.cmds.lock().unwrap().extend([Cmd::FindOpen, Cmd::FindNext]);
        }
        _ => {}
    }
    let size = run.write.size.clone();
    let mut app = App::new_with_flag(run.write.clone(), dirty);
    let _ = win.paint(&mut app);

    // Files the user opens in Write from Scrinium (opening-files.md §6.2).
    let mut channel = match atrium_open::Channel::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("atrium-write: the document channel: {e}");
            None
        }
    };
    let mut asking: Option<Asking> = None;
    let mut embed = None;
    let mut want_picker: Option<Asking> = None;
    let mut last_sync = Instant::now();
    // Closing waits on Save As (true), and Don't Save drops the changes.
    let mut close_after_save = false;
    let mut discard = false;
    'main: loop {
        match win.step(&mut app, Some(Duration::from_millis(if asking.is_some() { 20 } else { 250 }))) {
            // Unsaved changes: ask first. Asked to close again while asking (or
            // while picking), the changes stay in the autosave folder.
            Ok(StepResult::Closed) if run.write.doc.borrow().dirty && !run.write.closing.get() && asking.is_none() => {
                run.write.closing.set(true);
                run.write.changed();
            }
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
            app.invalidate();
        }
        for c in run.write.take_cmds() {
            match c {
                Cmd::Open => want_picker = Some(Asking::Open),
                Cmd::PasswordDone => run.password_done(),
                Cmd::PasswordClose => run.password_cancelled(),
                Cmd::CompareWith => want_picker = Some(Asking::Compare),
                Cmd::MergeWith => want_picker = Some(Asking::Merge),
                Cmd::InsertPicture => want_picker = Some(Asking::Picture),
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::ExportPdf => want_picker = Some(Asking::ExportPdf),
                Cmd::Save => {
                    if !run.save() {
                        want_picker = Some(Asking::SaveAs);
                    }
                }
                Cmd::CloseSave => {
                    if run.save() {
                        if !run.write.doc.borrow().dirty {
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
                _ => {}
            }
        }
        if let (Some(what), None) = (want_picker.take(), asking) {
            match (&embeds, win.embed_token()) {
                (Some(e), Some(token)) => {
                    let name = run.write.doc.borrow().name.clone();
                    let filter = match what {
                        Asking::Open => Filter { kind: Kind::File, extensions: ["owrite", "docx", "odt", "txt", "md", "text"].map(String::from).to_vec(), multiple: false, write: true, title: "Open a document".into(), name: String::new(), origin: String::new() },
                        Asking::Compare => Filter { kind: Kind::File, extensions: ["owrite", "docx", "odt", "txt", "md", "text"].map(String::from).to_vec(), multiple: false, write: false, title: format!("Compare “{}” with an earlier version", stem_of(&name)), name: String::new(), origin: String::new() },
                        Asking::Merge => Filter { kind: Kind::File, extensions: ["owrite", "docx", "odt", "txt", "md", "text"].map(String::from).to_vec(), multiple: false, write: false, title: format!("Merge a reviewed copy into “{}”", stem_of(&name)), name: String::new(), origin: String::new() },
                        Asking::Picture => Filter { kind: Kind::File, extensions: ["png", "jpg", "jpeg", "gif", "webp"].map(String::from).to_vec(), multiple: false, write: false, title: "Insert a picture".into(), name: String::new(), origin: String::new() },
                        Asking::SaveAs => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to save “{}”", stem_of(&name)), name: String::new(), origin: String::new() },
                        Asking::ExportPdf => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to put “{}.pdf”", stem_of(&name)), name: String::new(), origin: String::new() },
                    };
                    let asked = e.borrow_mut().request("picker", token, PICKER_SLOT);
                    match asked.and_then(|id| e.borrow_mut().send(id, ToChild::Open(filter).encode()).map(|_| id)) {
                        Ok(id) => {
                            embed = Some(id);
                            asking = Some(what);
                            run.write.ui.set(Ui { asking: true });
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
            match ev {
                Ok(Some(ParentEvent::MessageFd { bytes, .. })) => {
                    let fd = e.borrow_mut().take_fd();
                    match (ToParent::decode(&bytes), fd) {
                        (Some(ToParent::File { name, write, .. }), Some(fd)) if asking == Some(Asking::Open) => run.opened(name, write, fd),
                        (Some(ToParent::File { name, .. }), Some(fd)) if asking == Some(Asking::Merge) => {
                            match read_all(&mut File::from(fd)).map_err(|e| format!("Couldn't read “{name}”: {e}")).and_then(|data| load(&name, &data)) {
                                Ok(other) => run.write.merge_with(&other, &name),
                                Err(e) => run.set_status(e),
                            }
                        }
                        (Some(ToParent::File { name, .. }), Some(fd)) if asking == Some(Asking::Compare) => {
                            match read_all(&mut File::from(fd)).map_err(|e| format!("Couldn't read “{name}”: {e}")).and_then(|data| load(&name, &data)) {
                                Ok(older) => run.write.compare_with(&older, &name),
                                Err(e) => run.set_status(e),
                            }
                        }
                        (Some(ToParent::File { name, .. }), Some(fd)) if asking == Some(Asking::Picture) => {
                            let r = read_all(&mut File::from(fd)).map_err(|e| e.to_string()).and_then(|b| run.write.insert_picture(b));
                            if let Err(e) = r {
                                run.set_status(format!("Couldn't insert “{name}”: {e}"));
                            }
                        }
                        _ => eprintln!("atrium-write: an odd message with a descriptor from the picker"),
                    }
                }
                Ok(Some(ParentEvent::Message { bytes, .. })) => match ToParent::decode(&bytes) {
                    Some(ToParent::Folder { at, read_only, name }) if matches!(asking, Some(Asking::SaveAs | Asking::ExportPdf)) => {
                        if read_only {
                            run.set_status(format!("“{name}” is read-only; choose another folder."));
                        } else if asking == Some(Asking::ExportPdf) {
                            run.export_pdf(Path::new(&at));
                        } else {
                            run.save_into(Path::new(&at));
                        }
                    }
                    Some(ToParent::Done { .. }) | Some(ToParent::Cancelled) => finish = true,
                    Some(ToParent::Failed { why }) => {
                        run.set_status(format!("The picker couldn't hand that over: {why}"));
                        finish = true;
                    }
                    _ => {}
                },
                Ok(Some(ParentEvent::ChildLost { .. })) => finish = true,
                Ok(_) => {}
                Err(err) => {
                    eprintln!("atrium-write: limend: {err:?}");
                    finish = asking.is_some();
                }
            }
            if finish {
                if let Some(id) = embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                asking = None;
                run.write.ui.set(Ui { asking: false });
                // Saved on the way out: close; cancelled: stay open.
                if std::mem::take(&mut close_after_save) && !run.write.doc.borrow().dirty {
                    break 'main;
                }
            }
        }
        if let Some(ch) = channel.as_mut() {
            match ch.recv(Some(Duration::ZERO)) {
                Ok(Some(o)) => {
                    eprintln!("atrium-write: “{}” opened from Scrinium{}", o.name, if o.write { " (write)" } else { "" });
                    run.opened(o.name, o.write, o.file.into());
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("atrium-write: the document channel: {e}");
                    channel = None;
                }
            }
        }
        run.poll_export();
        if last_sync.elapsed() >= SYNC_EVERY {
            last_sync = Instant::now();
            let mut d = run.write.doc.borrow_mut();
            if let Some(a) = d.autosave() {
                if let Err(e) = a.sync() {
                    d.status = format!("Autosave failed: {e}");
                }
            }
        }
    }
    // Closing: a saved document leaves nothing behind; unsaved changes stay in the
    // autosave folder and come back at the next start.
    let d = std::mem::replace(&mut *run.write.doc.borrow_mut(), Doc::new(""));
    if d.dirty && !discard {
        let mut d = d;
        if let Some(a) = d.autosave() {
            let _ = a.sync();
        }
    } else {
        let _ = d.close();
    }
}
