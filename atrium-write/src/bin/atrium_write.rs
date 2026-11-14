//! atrium-write: the word processor (docs/spec/officina.md §7, O3c).
//!
//!   atrium-write          a new document (or the one a crash left, restored)
//!   atrium-write FILE     open FILE by path (development; apps get files from
//!                         the picker)
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
use atrium_write::files::{encode, format_of, load, stem_of, Format};
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
    SaveAs,
    Picture,
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
    let stem: String = stem.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let stem = stem.trim_start_matches('.');
    let stem = if stem.is_empty() { "Document" } else { stem };
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    for n in 1..1000 {
        let name = if n == 1 { format!("{stem}.owrite") } else { format!("{stem} {n}.owrite") };
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


struct Run {
    write: Write,
    target: Option<Target>,
    root: PathBuf,
}

impl Run {
    fn set_status(&self, s: impl Into<String>) {
        self.write.doc.borrow_mut().status = s.into();
        self.write.changed();
    }

    /// Puts `d` on screen with its own autosave folder (a restored one keeps its own). The document it replaces
    /// closes cleanly if saved; with unsaved changes its autosave folder stays, and
    /// it comes back at the next start.
    fn show(&mut self, mut d: Doc, target: Option<Target>) {
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
                d.status = if format == Format::Text {
                    format!("Saved “{}” as plain text: formatting is not kept in a text file.", d.name)
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

    /// A picked file arrived.
    fn opened(&mut self, name: String, write: bool, fd: std::os::fd::OwnedFd) {
        let mut f = File::from(fd);
        match read_all(&mut f).map_err(|e| format!("Couldn't read “{name}”: {e}")).and_then(|data| load(&name, &data)) {
            Ok(mut d) => {
                let format = format_of(&name);
                if !write {
                    d.status = format!("{} It opened read-only: Save asks where to put a copy.", d.status);
                }
                self.show(d, write.then_some(Target::Picked { file: f, format }));
            }
            Err(e) => self.set_status(e),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let root = state_root();
    let _ = fs::create_dir_all(&root);
    // A document a crash left comes back first.
    let mut first: Option<(Doc, Option<Target>)> = None;
    if let Some((dir, name)) = doc::pending(&root).pop() {
        match doc::recover(&dir) {
            Ok((mut d, rec)) => {
                d.status = format!("Restored “{name}” as it was before Write last stopped ({} changes since its last save).", rec.actions);
                first = Some((d, None));
            }
            Err(e) => eprintln!("atrium-write: couldn't restore {}: {e}", dir.display()),
        }
    }
    if first.is_none() {
        if let Some(path) = args.get(1) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
            match fs::read(&p).map_err(|e| e.to_string()).and_then(|data| load(&name, &data)) {
                Ok(d) => first = Some((d, Some(Target::Path { path: p, format: format_of(&name) }))),
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
    let mut run = Run { write, target: None, root: root.clone() };
    run.show(doc, target);
    let size = run.write.size.clone();
    let mut app = App::new_with_flag(run.write.clone(), dirty);
    let _ = win.paint(&mut app);

    let mut asking: Option<Asking> = None;
    let mut embed = None;
    let mut want_picker: Option<Asking> = None;
    let mut last_sync = Instant::now();
    loop {
        match win.step(&mut app, Some(Duration::from_millis(if asking.is_some() { 20 } else { 250 }))) {
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
                Cmd::InsertPicture => want_picker = Some(Asking::Picture),
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::Save => {
                    if !run.save() {
                        want_picker = Some(Asking::SaveAs);
                    }
                }
                _ => {}
            }
        }
        if let (Some(what), None) = (want_picker.take(), asking) {
            match (&embeds, win.embed_token()) {
                (Some(e), Some(token)) => {
                    let name = run.write.doc.borrow().name.clone();
                    let filter = match what {
                        Asking::Open => Filter { kind: Kind::File, extensions: ["owrite", "txt", "md", "text"].map(String::from).to_vec(), multiple: false, write: true, title: "Open a document".into() },
                        Asking::Picture => Filter { kind: Kind::File, extensions: ["png", "jpg", "jpeg", "gif", "webp"].map(String::from).to_vec(), multiple: false, write: false, title: "Insert a picture".into() },
                        Asking::SaveAs => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to save “{}”", stem_of(&name)) },
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
            }
        }
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
    if d.dirty {
        let mut d = d;
        if let Some(a) = d.autosave() {
            let _ = a.sync();
        }
    } else {
        let _ = d.close();
    }
}
