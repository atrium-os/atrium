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
use atrium_slides::files::{encode, format_of, load_file, picture_mime, stem_of, Format};
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

struct Run {
    slides: Slides,
    target: Option<Target>,
    root: PathBuf,
}

impl Run {
    fn set_status(&self, s: impl Into<String>) {
        self.slides.doc.borrow_mut().status = s.into();
        self.slides.changed();
    }

    /// Puts `d` on screen with its own autosave folder. The deck it replaces
    /// closes cleanly if saved; unsaved changes stay in its folder and come
    /// back at the next start.
    fn show(&mut self, mut d: Doc, target: Option<Target>) {
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

    /// A picked deck arrived.
    fn opened(&mut self, name: String, write: bool, fd: std::os::fd::OwnedFd) {
        let mut f = File::from(fd);
        match load_file(&name, &mut f, MAX_FILE) {
            Ok(mut d) => {
                let format = format_of(&name);
                if !write {
                    d.status = format!("{} It opened read-only: Save asks where to put a copy.", d.status).trim().to_string();
                }
                // Save writes the file in its own format; a file Slides can't
                // write asks where to put an .oslides.
                self.show(d, (write && format != Format::Other).then_some(Target::Picked { file: f, format }));
            }
            Err(e) => self.set_status(e),
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
    if let Some((dir, name)) = doc::pending(&root).pop() {
        match doc::recover(&dir) {
            Ok((mut d, rec)) => {
                d.status = format!("Restored “{name}” as it was before Slides last stopped ({} changes since its last save).", rec.actions);
                first = Some((d, None));
            }
            Err(e) => eprintln!("atrium-slides: couldn't restore {}: {e}", dir.display()),
        }
    }
    if first.is_none() {
        if let Some(path) = args.get(1) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
            match File::open(&p).map_err(|e| e.to_string()).and_then(|mut f| load_file(&name, &mut f, MAX_FILE)) {
                Ok(d) => {
                    let format = format_of(&name);
                    first = Some((d, (format != Format::Other).then_some(Target::Path { path: p, format })));
                }
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
    let mut run = Run { slides, target: None, root: root.clone() };
    run.show(doc, target);
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
    loop {
        match win.step(&mut app, Some(Duration::from_millis(if asking.is_some() { 20 } else { 250 }))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if size.get() != (win.width, win.height) || run.slides.scale.get() != win.scale() as f64 {
            size.set((win.width, win.height));
            run.slides.scale.set(win.scale() as f64);
            app.invalidate();
        }
        for c in run.slides.take_cmds() {
            match c {
                Cmd::Open => want_picker = Some(Asking::Open),
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::Picture => want_picker = Some(Asking::Picture),
                Cmd::Present(cur) => present = Some(cur),
                Cmd::Save => {
                    if !run.save() {
                        want_picker = Some(Asking::SaveAs);
                    }
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
                        Asking::Open => Filter { kind: Kind::File, extensions: ["oslides", "pptx", "odp"].map(String::from).to_vec(), multiple: false, write: true, title: "Open a presentation".into() },
                        Asking::SaveAs => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to save “{}”", stem_of(&name)) },
                        Asking::Picture => Filter { kind: Kind::File, extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg"].map(String::from).to_vec(), multiple: false, write: false, title: "Insert a picture".into() },
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
                    Some(ToParent::Folder { at, read_only, name }) if asking == Some(Asking::SaveAs) => {
                        if read_only {
                            run.set_status(format!("“{name}” is read-only; choose another folder."));
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
    if d.dirty {
        if let Some(a) = d.autosave() {
            let _ = a.sync();
        }
    } else {
        let _ = d.close();
    }
}
