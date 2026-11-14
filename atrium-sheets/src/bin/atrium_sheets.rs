//! atrium-sheets: the spreadsheet app (docs/spec/officina.md §8, O2).
//!
//!   atrium-sheets                 a new sheet (or the one a crash left, restored)
//!   atrium-sheets FILE            open FILE by path (development; apps get files
//!                                 from the picker)
//!   atrium-sheets --bench PATTERN ROWS SECS [HZ]
//!                                 the O2 scroll gate, live: a ROWS-row sheet driven by
//!                                 the scripted PATTERN (swipe, diagonal, wheel, page,
//!                                 jump, or all) on the wall clock; logs one report line
//!                                 per pattern and exits
//!
//! Files come from the system picker (scrinium.md §4.1): Open asks for one file,
//! read-write; Save writes back to it. Save As asks for a folder with write access and
//! writes `NAME.osheet` there (temp file, then rename), until the picker's own Save As
//! sheet exists (§4.2). Every change is journaled to a private autosave folder
//! (`$XDG_STATE_HOME` or `~/.local/state`, `atrium-sheets/autosave/`); a document that
//! did not close cleanly is restored at the next start.
//!
//! FRESCO_SOCKET and LIMEN_SOCKET as usual; without limend the app runs, and Open and
//! Save As say the picker is not available.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_sheets::app::{Cmd, Sheets, Ui, PICKER_SLOT};
use atrium_sheets::bench::{self, Frame, Pattern, Report};
use atrium_sheets::doc::{self, Doc};
use atrium_sheets::files::{format_of, load, stem_of, Format};
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
    /// A path (development, or written by Save As).
    Path { path: PathBuf, format: Format },
}

/// What the picker is up for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asking {
    Open,
    SaveAs,
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

fn encode(d: &Doc, format: Format) -> io::Result<Vec<u8>> {
    match format {
        Format::Native => d.write_native(Vec::new()),
        Format::Csv => d.write_csv(Vec::new()),
        Format::Xlsx => officina_convert::xlsx::write(&d.wb, Vec::new()),
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

/// Writes `bytes` to `dir/name` through a temp file and a rename; never replaces a
/// file already there (`name 2.osheet` and so on instead). Returns the path.
fn write_new(dir: &Path, stem: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let stem: String = stem.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect();
    let stem = stem.trim_start_matches('.');
    let stem = if stem.is_empty() { "Sheet" } else { stem };
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    for n in 1..1000 {
        let name = if n == 1 { format!("{stem}.osheet") } else { format!("{stem} {n}.osheet") };
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
    sheets: Sheets,
    target: Option<Target>,
    root: PathBuf,
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
        if d.autosave().is_none() {
            if let Err(e) = d.autosave_to(&new_autosave_dir(&self.root)) {
                d.status = format!("{} Autosave is off: {e}.", d.status);
            }
        }
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
                d.status = if lost { format!("Saved “{}” as CSV: values only, formulas are not kept in CSV.", d.name) } else { format!("Saved “{}”.", d.name) };
            }
            Err(e) => d.status = format!("Couldn't save “{}”: {e}. Your changes are still here and in the autosave.", d.name),
        }
        drop(d);
        self.sheets.changed();
        true
    }

    fn save_into(&mut self, dir: &Path) {
        let (stem, bytes) = {
            let d = self.sheets.doc.borrow();
            (stem_of(&d.name).to_string(), encode(&d, Format::Native))
        };
        let r = bytes.and_then(|b| write_new(dir, &stem, &b));
        match r {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                {
                    let mut d = self.sheets.doc.borrow_mut();
                    d.name = name.clone();
                    let _ = d.saved();
                    d.status = format!("Saved as “{name}”.");
                }
                self.target = Some(Target::Path { path, format: Format::Native });
                self.sheets.changed();
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
    if args.get(1).map(String::as_str) == Some("--bench") {
        std::process::exit(bench_main(&args[2..]));
    }

    let root = state_root();
    let _ = fs::create_dir_all(&root);
    // A document a crash left comes back first.
    let mut first: Option<(Doc, Option<Target>)> = None;
    if let Some((dir, name)) = doc::pending(&root).pop() {
        match doc::recover(&dir) {
            Ok((mut d, rec)) => {
                d.status = format!("Restored “{name}” as it was before Sheets last stopped ({} changes since its last save).", rec.actions);
                first = Some((d, None));
            }
            Err(e) => eprintln!("atrium-sheets: couldn't restore {}: {e}", dir.display()),
        }
    }
    if first.is_none() {
        if let Some(path) = args.get(1) {
            let p = PathBuf::from(path);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
            match fs::read(&p).map_err(|e| e.to_string()).and_then(|data| load(&name, &data)) {
                Ok(d) => first = Some((d, Some(Target::Path { path: p, format: format_of(&name) }))),
                Err(e) => eprintln!("atrium-sheets: {e}"),
            }
        }
    }
    let (doc, target) = first.unwrap_or_else(|| (Doc::new("Untitled"), None));

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
    let sheets = Sheets::new(
        Doc::new(""),
        Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        Mutable::with_dirty(GridState::default(), dirty.clone()),
        Mutable::with_dirty(Ui::default(), dirty.clone()),
        Mutable::with_dirty(0u64, dirty.clone()),
        (win.width, win.height),
    );
    let mut run = Run { sheets, target: None, root: root.clone() };
    run.show(doc, target);
    let size = run.sheets.size.clone();
    let mut app = App::new_with_flag(run.sheets.clone(), dirty);
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
        for c in run.sheets.take_cmds() {
            match c {
                Cmd::Open => want_picker = Some(Asking::Open),
                Cmd::SaveAs => want_picker = Some(Asking::SaveAs),
                Cmd::Save => {
                    if !run.save() {
                        want_picker = Some(Asking::SaveAs);
                    }
                }
                Cmd::Undo | Cmd::Redo => {}
            }
        }
        if let (Some(what), None) = (want_picker.take(), asking) {
            match (&embeds, win.embed_token()) {
                (Some(e), Some(token)) => {
                    let name = run.sheets.doc.borrow().name.clone();
                    let filter = match what {
                        Asking::Open => Filter { kind: Kind::File, extensions: ["xlsx", "xlsm", "csv", "tsv", "txt", "osheet"].map(String::from).to_vec(), multiple: false, write: true, title: "Open a spreadsheet".into() },
                        Asking::SaveAs => Filter { kind: Kind::Folder, extensions: Vec::new(), multiple: false, write: true, title: format!("Choose where to save “{}”", stem_of(&name)) },
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
            match ev {
                Ok(Some(ParentEvent::MessageFd { bytes, .. })) => {
                    let fd = e.borrow_mut().take_fd();
                    match (ToParent::decode(&bytes), fd) {
                        (Some(ToParent::File { name, write, .. }), Some(fd)) if asking == Some(Asking::Open) => run.opened(name, write, fd),
                        _ => eprintln!("atrium-sheets: an odd message with a descriptor from the picker"),
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
                    eprintln!("atrium-sheets: limend: {err:?}");
                    finish = asking.is_some();
                }
            }
            if finish {
                if let Some(id) = embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                asking = None;
                run.sheets.ui.set(Ui { asking: false });
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
    if d.dirty {
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
            }
        }
        let r = Report::new(&frames, period, &moving);
        all_pass &= r.passes(period);
        println!("atrium-sheets bench {} rows {rows} hz {hz}: {}", p.name(), r.line(period));
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
