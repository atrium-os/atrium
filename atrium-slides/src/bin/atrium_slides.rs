//! atrium-slides: the presentation app (docs/spec/officina.md §9, O6).
//!
//!   atrium-slides          a new deck (or the one a crash left, restored)
//!   atrium-slides FILE     open FILE by path (development; apps get files
//!                          from the picker)
//!
//! F5 (or Present) shows the deck full screen from the first slide,
//! Shift+F5 from the current one (`atrium_slides::present`). For tests,
//! `ATRIUM_SLIDES_PRESENT=1` presents at once and quits when the show ends,
//! and `ATRIUM_SLIDES_PRESS_MS=N` presses Next every N ms.
//!
//! Files come from the system picker (scrinium.md §4.1): Open asks for one
//! file, read-write (`.oslides`, or `.pptx`/`.odp` through the sandboxed
//! converter); Save writes back to it in its own format; Save As asks for a folder and
//! writes `NAME.oslides` there (temp file, then a link that never replaces);
//! Picture asks for one picture, read-only. Every change is journaled to a
//! private autosave folder (`$XDG_STATE_HOME` or `~/.local/state`,
//! `atrium-slides/autosave/`); a deck that did not close cleanly is restored
//! at the next start.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_slides::app::{Cmd, Slides, Ui, PICKER_SLOT};
use atrium_slides::doc::{self, Doc};
use atrium_slides::files::{encode, format_of, load_file_with, picture_mime, stem_of, Format};
use officina_convert::launch::ConvertError;
use atrium_slides::mapped::copy_file;
use limen::picker::{Filter, Kind, ToChild, ToParent};
use limen::proto::ParentEvent;
use pergola::embed::Embeds;
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};

/// The largest file Open reads.
const MAX_FILE: u64 = 4 << 30;
/// The largest picture Picture reads.
const MAX_PICTURE: u64 = doc::MAX_MEDIA as u64;
/// How often the journal is made durable.
const SYNC_EVERY: Duration = Duration::from_secs(2);

/// Where the user's file is, for Save.
/// Where Save writes, and as what.
enum Target {
    Picked { file: File, format: Format },
    Path { path: PathBuf, format: Format },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Asking {
    Open,
    SaveAs,
    Picture,
    /// A folder for Reduce File Size's copy.
    Reduce,
}

fn state_root() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("atrium-slides/autosave")
}

fn new_autosave_dir(root: &Path) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    root.join(format!("{}-{n}", std::process::id()))
}

fn read_all(f: &mut File, max: u64) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the file is over {} MB", max >> 20)));
    }
    Ok(data)
}

/// Writes the deck over a picked file. The autosave folder still holds it
/// until this returns, so a crash halfway loses nothing.
fn write_over(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Writes `bytes` to `dir` as `stem.oslides` through a temp file and a hard
/// link; never replaces a file already there (`stem 2.oslides` and so on).
fn write_new(dir: &Path, stem: &str, src: &File) -> io::Result<PathBuf> {
    let stem: String = stem.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let stem = stem.trim_start_matches('.');
    let stem = if stem.is_empty() { "Presentation" } else { stem };
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        if let Err(e) = copy_file(src, &mut f) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    for n in 1..1000 {
        let name = if n == 1 { format!("{stem}.oslides") } else { format!("{stem} {n}.oslides") };
        let path = dir.join(&name);
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
    File { name: String, file: File, target: Option<Target>, read_only: bool },
    /// A protected deck's autosave folder, to restore.
    Recovery { dir: PathBuf, name: String },
}

struct Run {
    slides: Slides,
    target: Option<Target>,
    locked: Option<Locked>,
    root: PathBuf,
    /// Rehearse or Reduce File Size running on its own copy of the deck.
    background: Option<Background>,
}

/// Work on a thread: its progress line and, once done, its report.
struct Background {
    what: &'static str,
    done: Arc<std::sync::atomic::AtomicUsize>,
    of: Arc<std::sync::atomic::AtomicUsize>,
    rx: std::sync::mpsc::Receiver<String>,
}

impl Background {
    fn start(what: &'static str, f: impl FnOnce(&dyn Fn(usize, usize)) -> String + Send + 'static) -> Background {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (done, of) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (tx, rx) = std::sync::mpsc::channel();
        let (d, o) = (done.clone(), of.clone());
        let _ = std::thread::Builder::new().name(what.into()).spawn(move || {
            let msg = f(&|a, b| {
                d.store(a, Ordering::Relaxed);
                o.store(b, Ordering::Relaxed);
            });
            let _ = tx.send(msg);
        });
        Background { what, done, of, rx }
    }
    fn poll(&self) -> Option<String> {
        match self.rx.try_recv() {
            Ok(m) => Some(m),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(format!("{} stopped.", self.what)),
            Err(_) => None,
        }
    }
    fn progress(&self) -> String {
        use std::sync::atomic::Ordering;
        format!("{}… {} of {}", self.what, self.done.load(Ordering::Relaxed), self.of.load(Ordering::Relaxed))
    }
}

impl Run {
    fn set_status(&self, s: impl Into<String>) {
        self.slides.doc.borrow_mut().status = s.into();
        self.slides.changed();
    }

    /// Puts `d` on screen with its own autosave folder. The deck it replaces
    /// closes cleanly if saved; unsaved changes stay in its folder and come
    /// back at the next start.
    fn show(&mut self, mut d: Doc, mut target: Option<Target>) {
        // Slides reads PowerPoint 97–2003 files but doesn't write them: Save asks where to keep a copy.
        if matches!(target, Some(Target::Picked { format: Format::Ppt, .. } | Target::Path { format: Format::Ppt, .. })) {
            d.status = format!("{} Slides reads PowerPoint 97–2003 files but doesn't write them: Save asks where to keep a copy.", d.status);
            target = None;
        }
        if d.autosave().is_none() {
            if let Err(e) = d.autosave_to(&new_autosave_dir(&self.root)) {
                d.status = format!("{} Autosave is off: {e}.", d.status);
            }
        }
        let mut old = self.slides.replace(d);
        self.target = target;
        if old.dirty {
            let name = old.name.clone();
            if let Some(a) = old.autosave() {
                let _ = a.sync();
            }
            drop(old);
            let s = self.slides.doc.borrow().status.clone();
            self.set_status(format!("{s} Unsaved changes to “{name}” are kept and come back next time Slides starts.").trim().to_string());
        } else {
            let _ = old.close();
        }
    }

    /// Saves to the file the deck came from; false when there is none. A
    /// native deck is written to a new file first and copied over the old
    /// one (the deck is mapped from the file it was opened from, which must
    /// not change under it); other formats are encoded in memory.
    fn save(&mut self) -> bool {
        let Some(t) = self.target.as_mut() else { return false };
        let format = match t {
            Target::Picked { format, .. } | Target::Path { format, .. } => *format,
        };
        let mut d = self.slides.doc.borrow_mut();
        let into = |t: &mut Target, f: &mut dyn FnMut(&mut File) -> io::Result<()>| match t {
            Target::Picked { file, .. } => f(file),
            Target::Path { path, .. } => f(&mut OpenOptions::new().write(true).open(&*path)?),
        };
        let r = if matches!(format, Format::Native | Format::Other) {
            let spool = d.spool_path();
            d.spool(&spool).and_then(|src| into(t, &mut |dst| copy_file(&src, dst).map(|_| ()))).and_then(|_| d.saved_as(&spool))
        } else {
            encode(&mut d, format).and_then(|bytes| into(t, &mut |dst| write_over(dst, &bytes))).and_then(|_| d.saved())
        };
        match r {
            Ok(()) => d.status = format!("Saved “{}”.", d.name),
            Err(e) => d.status = format!("Couldn't save “{}”: {e}. Your changes are still here and in the autosave.", d.name),
        }
        drop(d);
        self.slides.changed();
        true
    }

    fn save_into(&mut self, dir: &Path) {
        let r = {
            let mut d = self.slides.doc.borrow_mut();
            let stem = stem_of(&d.name).to_string();
            let spool = d.spool_path();
            d.spool(&spool).and_then(|src| write_new(dir, &stem, &src)).map(|p| (p, spool))
        };
        match r {
            Ok((path, spool)) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                {
                    let mut d = self.slides.doc.borrow_mut();
                    d.name = name.clone();
                    let _ = d.saved_as(&spool);
                    d.status = format!("Saved as “{name}”.");
                }
                self.target = Some(Target::Path { path, format: Format::Native });
                self.slides.changed();
            }
            Err(e) => self.set_status(format!("Couldn't save there: {e}.")),
        }
    }

    /// Rehearse on a copy of the deck, on a thread.
    fn rehearse(&mut self) {
        if self.background.is_some() {
            return self.set_status("Wait for the work running to finish.");
        }
        let mut deck = self.slides.doc.borrow().deck.clone();
        self.background = Some(Background::start("Rehearsing", move |progress| {
            let costs = atrium_slides::tools::rehearse(&mut deck, (1920, 1080), |a, b| progress(a, b));
            atrium_slides::tools::rehearse_summary(&costs)
        }));
    }

    /// Reduce File Size into a new file in `dir`, on a thread.
    fn reduce_into(&mut self, dir: PathBuf) {
        if self.background.is_some() {
            return self.set_status("Wait for the work running to finish.");
        }
        let (mut deck, stem, sealer) = {
            let d = self.slides.doc.borrow();
            (d.deck.clone(), format!("{} (smaller)", stem_of(&d.name)), d.protection.as_ref().map(|p| p.sealer.clone()))
        };
        self.background = Some(Background::start("Making pictures smaller", move |progress| {
            let r = atrium_slides::tools::reduce(&mut deck, 1920, |a, b| progress(a, b));
            let tmp = dir.join(format!(".{}.reduce.tmp", std::process::id()));
            let written = (|| {
                let f = OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
                // A protected deck's copy is sealed with the same password.
                let f = match &sealer {
                    Some(s) => {
                        let mut f = f;
                        f.write_all(&s.seal(&officina_slides::native::save_to_vec(&deck)))?;
                        f
                    }
                    None => officina_slides::native::save(&deck, io::BufWriter::with_capacity(1 << 20, f))?.into_inner().map_err(|e| e.into_error())?,
                };
                f.sync_all()?;
                let p = write_new(&dir, &stem, &f);
                let _ = fs::remove_file(&tmp);
                p
            })();
            match written {
                Ok(p) => format!("{} Saved as “{}”.", atrium_slides::tools::reduce_summary(&r), p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    format!("The smaller copy couldn't be saved: {e}.")
                }
            }
        }));
    }

    /// A picked deck arrived.
    fn opened(&mut self, name: String, write: bool, fd: std::os::fd::OwnedFd) {
        let f = File::from(fd);
        let format = format_of(&name);
        // Save writes the file in its own format; a file Slides can't write
        // asks where to put an .oslides.
        let target = (write && format != Format::Other).then(|| f.try_clone().ok().map(|file| Target::Picked { file, format })).flatten();
        self.open_file(name, f, target, !write, None);
    }

    /// Opens `f` (named `name`), with `password` when it is protected;
    /// without the right one, the password bar asks and the file waits.
    fn open_file(&mut self, name: String, mut f: File, target: Option<Target>, read_only: bool, password: Option<&str>) {
        match load_file_with(&name, &mut f, MAX_FILE, password) {
            Ok(mut d) => {
                if read_only {
                    d.status = format!("{} It opened read-only: Save asks where to put a copy.", d.status).trim().to_string();
                }
                self.show(d, target);
            }
            Err(e @ (ConvertError::NeedsPassword | ConvertError::WrongPassword)) => {
                let again = e == ConvertError::WrongPassword;
                self.slides.ask_password(&if again { format!("Wrong password for “{name}”. Try again") } else { format!("“{name}” is protected. Its password") }, false);
                self.locked = Some(Locked::File { name, file: f, target, read_only });
            }
            Err(ConvertError::Failed(e)) => self.set_status(e),
        }
    }

    /// Restores a protected deck's autosave folder, asking for its password.
    fn recover_locked(&mut self, dir: PathBuf, name: String, password: Option<&str>) {
        match password.map(|pw| doc::recover_with(&dir, Some(pw))) {
            Some(Ok((mut d, rec))) => {
                d.status = format!("Restored “{name}” as it was before Slides last stopped ({} changes since its last save).", rec.actions);
                self.show(d, None);
            }
            Some(Err(e)) if e.kind() != io::ErrorKind::PermissionDenied => self.set_status(format!("Couldn't restore “{name}”: {e}")),
            r => {
                let again = r.is_some();
                self.slides.ask_password(&if again { format!("Wrong password for “{name}”. Try again") } else { format!("Restore “{name}”: its password") }, false);
                self.locked = Some(Locked::Recovery { dir, name });
            }
        }
    }

    /// The password bar's answer: a waiting file opened, or the deck's
    /// password set.
    fn password_done(&mut self) {
        let (bar, text) = self.slides.take_password();
        if bar.setting {
            let pw = (!text.is_empty()).then_some(text.as_str());
            let r = self.slides.doc.borrow_mut().set_password(pw);
            let mut d = self.slides.doc.borrow_mut();
            d.dirty = true;
            d.status = match (r, pw) {
                (Err(e), _) => format!("The password is set, but autosave stopped: {e}."),
                (Ok(()), Some(_)) => format!("“{}” is protected with a password from its next save.", d.name),
                (Ok(()), None) => format!("“{}” has no password from its next save.", d.name),
            };
            drop(d);
            self.slides.changed();
            return;
        }
        match self.locked.take() {
            Some(Locked::File { name, file, target, read_only }) => self.open_file(name, file, target, read_only, Some(&text)),
            Some(Locked::Recovery { dir, name }) => self.recover_locked(dir, name, Some(&text)),
            None => {}
        }
    }

    fn password_cancelled(&mut self) {
        match self.locked.take() {
            Some(Locked::File { name, .. }) => self.set_status(format!("“{name}” stayed closed.")),
            Some(Locked::Recovery { name, .. }) => self.set_status(format!("“{name}” wasn't restored; it comes back next time Slides starts.")),
            None => {}
        }
    }

    /// A picked picture arrived.
    fn picture(&mut self, name: String, fd: std::os::fd::OwnedFd) {
        let mut f = File::from(fd);
        let r = read_all(&mut f, MAX_PICTURE).map_err(|e| format!("Couldn't read “{name}”: {e}")).and_then(|data| {
            let mime = picture_mime(&data).ok_or_else(|| format!("“{name}” isn't a picture Slides can show."))?;
            self.slides.insert_picture(mime, &data)
        });
        if let Err(e) = r {
            self.set_status(e);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = state_root();
    let _ = fs::create_dir_all(&root);
    let mut first: Option<(Doc, Option<Target>)> = None;
    let mut locked: Option<(PathBuf, String)> = None;
    let mut locked_file: Option<(String, File, Option<Target>)> = None;
    if let Some((dir, name)) = doc::pending(&root).pop() {
        if doc::recovery_locked(&dir) {
            locked = Some((dir, name));
        } else {
            match doc::recover(&dir) {
                Ok((mut d, rec)) => {
                    d.status = format!("Restored “{name}” as it was before Slides last stopped ({} changes since its last save).", rec.actions);
                    first = Some((d, None));
                }
                Err(e) => eprintln!("atrium-slides: couldn't restore {}: {e}", dir.display()),
            }
        }
    }
    if first.is_none() && locked.is_none() {
        if let Some(path) = args.get(1) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
            let format = format_of(&name);
            match File::open(&p) {
                Ok(mut f) => match load_file_with(&name, &mut f, MAX_FILE, None) {
                    Ok(d) => first = Some((d, (format != Format::Other).then_some(Target::Path { path: p, format }))),
                    Err(ConvertError::Failed(e)) => eprintln!("atrium-slides: {e}"),
                    Err(_) => locked_file = Some((name, f, (format != Format::Other).then_some(Target::Path { path: p.clone(), format }))),
                },
                Err(e) => eprintln!("atrium-slides: {e}"),
            }
        }
    }
    let (doc, target) = first.unwrap_or_else(|| (Doc::new("Presentation"), None));

    let mut win = match Window::open(&WindowDesc::new("Slides", SizeSpec::Fixed(1200, 720))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("atrium-slides: frescod: {e}");
            std::process::exit(1);
        }
    };
    let embeds = match Embeds::connect() {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("atrium-slides: no limend ({e:?}); the picker is not available");
            None
        }
    };
    if let Some(e) = &embeds {
        win.set_slot_binder(Embeds::binder(e));
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let slides = Slides::new(Doc::from_deck(officina_slides::model::Deck::new(), ""), Mutable::with_dirty(Ui::default(), dirty.clone()), Mutable::with_dirty(0u64, dirty.clone()), (win.width, win.height));
    slides.scale.set(win.scale() as f64);
    let mut run = Run { slides, target: None, locked: None, root: root.clone(), background: None };
    run.show(doc, target);
    if let Some((dir, name)) = locked {
        run.recover_locked(dir, name, None);
    } else if let Some((name, f, t)) = locked_file {
        run.open_file(name, f, t, false, None);
    }
    // Development: start on slide N (screenshots).
    if let Some(n) = std::env::var("ATRIUM_SLIDES_SLIDE").ok().and_then(|v| v.parse::<usize>().ok()) {
        run.slides.go(n.saturating_sub(1));
    }
    let size = run.slides.size.clone();
    let mut app = App::new_with_flag(run.slides.clone(), dirty);
    let _ = win.paint(&mut app);

    let mut present: Option<bool> = std::env::var_os("ATRIUM_SLIDES_PRESENT").map(|_| false);
    let quit_after_show = present.is_some();
    let press_every = std::env::var("ATRIUM_SLIDES_PRESS_MS").ok().and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis);
    let mut asking: Option<Asking> = None;
    let mut embed = None;
    let mut want_picker: Option<Asking> = None;
    let mut last_sync = Instant::now();
    // Closing waits on Save As (true), and Don't Save drops the changes.
    let mut close_after_save = false;
    let mut discard = false;
    'main: loop {
        let busy = atrium_slides::render::decoding() + atrium_slides::render::previewing() > 0 || run.background.is_some();
        match win.step(&mut app, Some(Duration::from_millis(if asking.is_some() || busy { 20 } else { 250 }))) {
            // Unsaved changes: ask first. Asked to close again while asking (or
            // while picking), the changes stay in the autosave folder.
            Ok(StepResult::Closed) if run.slides.doc.borrow().dirty && !run.slides.closing.get() && asking.is_none() => {
                run.slides.closing.set(true);
                run.slides.changed();
            }
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        // Pictures decoded and previews made off this thread (§9.3).
        let key = run.slides.deck_key();
        if atrium_slides::render::tend(&mut run.slides.doc.borrow_mut().deck, key) {
            app.invalidate();
        }
        if size.get() != (win.width, win.height) || run.slides.scale.get() != win.scale() as f64 {
            size.set((win.width, win.height));
            run.slides.scale.set(win.scale() as f64);
            app.invalidate();
        }
        for c in run.slides.take_cmds() {
            match c {
                Cmd::Open => want_picker = Some(Asking::Open),
                Cmd::PasswordDone => run.password_done(),
                Cmd::PasswordClose => run.password_cancelled(),
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::Picture => want_picker = Some(Asking::Picture),
                Cmd::Present(cur) => present = Some(cur),
                Cmd::Rehearse => run.rehearse(),
                Cmd::Reduce => want_picker = Some(Asking::Reduce),
                Cmd::Save => {
                    if !run.save() {
                        want_picker = Some(Asking::SaveAs);
                    }
                }
                Cmd::CloseSave => {
                    if run.save() {
                        if !run.slides.doc.borrow().dirty {
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
        if let (Some(cur), None) = (present.take(), asking) {
            let (deck, start) = {
                let d = run.slides.doc.borrow();
                (d.deck.clone(), if cur { d.current } else { 0 })
            };
            let shown = atrium_slides::present::present(deck, run.slides.deck_key(), run.slides.fonts(), start, embeds.clone(), press_every, || {
                let _ = win.step(&mut app, Some(Duration::ZERO));
            });
            match shown {
                Ok(r) => {
                    if quit_after_show {
                        let worst = r.animations.iter().map(|a| a.1).max().unwrap_or(0);
                        eprintln!("atrium-slides: show over at slide {}; {} animations, worst gap {worst} us", r.last + 1, r.animations.len());
                        break;
                    }
                    run.slides.go(r.last);
                }
                Err(e) => run.set_status(format!("The slide show didn't start: {e}")),
            }
        }
        if let (Some(what), None) = (want_picker.take(), asking) {
            match (&embeds, win.embed_token()) {
                (Some(e), Some(token)) => {
                    let name = run.slides.doc.borrow().name.clone();
                    let filter = match what {
                        Asking::Open => Filter { kind: Kind::File, extensions: ["oslides", "pptx", "odp"].map(String::from).to_vec(), multiple: false, write: true, title: "Open a presentation".into(), name: String::new(), origin: String::new() },
                        Asking::SaveAs => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to save “{}”", stem_of(&name)), name: String::new(), origin: String::new() },
                        Asking::Picture => Filter { kind: Kind::File, extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg"].map(String::from).to_vec(), multiple: false, write: false, title: "Insert a picture".into(), name: String::new(), origin: String::new() },
                        Asking::Reduce => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to save a smaller copy of “{}”", stem_of(&name)), name: String::new(), origin: String::new() },
                    };
                    let asked = e.borrow_mut().request("picker", token, PICKER_SLOT);
                    match asked.and_then(|id| e.borrow_mut().send(id, ToChild::Open(filter).encode()).map(|_| id)) {
                        Ok(id) => {
                            embed = Some(id);
                            asking = Some(what);
                            run.slides.ui.set(Ui { asking: true });
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
                        (Some(ToParent::File { name, .. }), Some(fd)) if asking == Some(Asking::Picture) => run.picture(name, fd),
                        _ => eprintln!("atrium-slides: an odd message with a descriptor from the picker"),
                    }
                }
                Ok(Some(ParentEvent::Message { bytes, .. })) => match ToParent::decode(&bytes) {
                    Some(ToParent::Folder { at, read_only, name }) if matches!(asking, Some(Asking::SaveAs | Asking::Reduce)) => {
                        if read_only {
                            run.set_status(format!("“{name}” is read-only; choose another folder."));
                        } else if asking == Some(Asking::Reduce) {
                            run.reduce_into(PathBuf::from(&at));
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
                    eprintln!("atrium-slides: limend: {err:?}");
                    finish = asking.is_some();
                }
            }
            if finish {
                if let Some(id) = embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                asking = None;
                run.slides.ui.set(Ui { asking: false });
                // Saved on the way out: close; cancelled: stay open.
                if std::mem::take(&mut close_after_save) && !run.slides.doc.borrow().dirty {
                    break 'main;
                }
            }
        }
        if let Some(msg) = run.background.as_ref().and_then(|b| b.poll()) {
            run.background = None;
            run.set_status(msg);
        } else if let Some(b) = &run.background {
            let p = b.progress();
            if p != run.slides.doc.borrow().status {
                run.set_status(p);
            }
        }
        if last_sync.elapsed() >= SYNC_EVERY {
            last_sync = Instant::now();
            let mut d = run.slides.doc.borrow_mut();
            if let Some(a) = d.autosave() {
                if let Err(e) = a.sync() {
                    d.status = format!("Autosave failed: {e}");
                }
            }
        }
    }
    // Closing: a saved deck leaves nothing behind; unsaved changes stay in the
    // autosave folder and come back at the next start.
    let mut d = std::mem::replace(&mut *run.slides.doc.borrow_mut(), Doc::from_deck(officina_slides::model::Deck::new(), ""));
    if d.dirty && !discard {
        if let Some(a) = d.autosave() {
            let _ = a.sync();
        }
    } else {
        let _ = d.close();
    }
}
