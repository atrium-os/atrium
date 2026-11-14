//! One open text document: the editor (document, undo, pages) and its autosave
//! (officina.md §4.4), the same scheme as atrium-sheets.
//!
//! **Autosave.** Each document has a private folder holding `base.<g>.owrite`
//! (the document as opened or last made a base) and `journal.<g>`: every action
//! since, appended as it happens (`officina_core::journal`, length and CRC per
//! record). A clean close removes the folder; after a crash [`recover`] loads
//! the base and replays the journal through the editor, so the document comes
//! back as it was on screen, undo steps included. Nothing reaches the user's
//! own file without Save (D9).
//!
//! A journal record is one editor action ([`Logged`]): the ops as applied,
//! whether they joined the action before (a run of typing), and the character
//! attributes interned for them, so the replayed attribute table numbers them
//! the same.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use officina_core::journal::{self, JournalWriter};
use officina_core::wire::{Reader, WireError, Writer};
use officina_text::layout::{Fonts, PinnedFonts};
use officina_text::model::TextDoc;
use officina_text::native;
use pergola_richtext::{Editor, Logged};

/// Undo steps kept.
pub const UNDO_LIMIT: usize = 1000;
/// Largest plain-text file opened, in bytes.
pub const MAX_TEXT_FILE: usize = 256 << 20;

const CURRENT: &str = "current";
const META: &str = "name";
const LOCK: &str = "lock";

fn base_path(dir: &Path, g: u64) -> PathBuf {
    dir.join(format!("base.{g}.owrite"))
}

pub fn journal_path(dir: &Path) -> io::Result<PathBuf> {
    Ok(dir.join(format!("journal.{}", generation(dir)?)))
}

fn generation(dir: &Path) -> io::Result<u64> {
    fs::read_to_string(dir.join(CURRENT))?.trim().parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad generation"))
}

fn lock(dir: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK))?;
    // SAFETY: flock on a descriptor we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "another window has this document"));
    }
    Ok(f)
}

/// The pinned fonts, shared by every document in the process.
pub fn fonts() -> Rc<dyn Fonts> {
    thread_local! {
        static F: Rc<dyn Fonts> = Rc::new(PinnedFonts::new().expect("the pinned fonts load"));
    }
    F.with(Rc::clone)
}

#[derive(Debug, Clone, PartialEq)]
enum Record {
    Action(Logged),
    Saved,
}

fn encode(r: &Record) -> Vec<u8> {
    let mut w = Writer::new();
    match r {
        Record::Action(Logged::Run { label, ops, merge, attrs }) => {
            w.u8(1);
            w.str(label);
            w.u8(*merge as u8);
            w.var(attrs.len() as u64);
            for a in attrs {
                native::encode_attrs(&mut w, a);
            }
            w.var(ops.len() as u64);
            for op in ops {
                w.bytes(&native::encode_op(op));
            }
        }
        Record::Action(Logged::Undo) => w.u8(2),
        Record::Action(Logged::Redo) => w.u8(3),
        Record::Saved => w.u8(5),
    }
    w.into_bytes()
}

/// Decodes a record against the editor's tables as they are now.
fn decode(b: &[u8], attrs: usize, styles: usize) -> Result<Record, WireError> {
    let mut r = Reader::new(b);
    let rec = match r.u8()? {
        1 => {
            let label = r.str("label", 256)?.to_string();
            let merge = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("merge")),
            };
            let n = r.count("attributes", native::MAX_ATTRS, 2)?;
            let mut new = Vec::with_capacity(n);
            for _ in 0..n {
                new.push(native::decode_attrs(&mut r)?);
            }
            let k = r.count("ops", 1 << 20, 1)?;
            let mut ops = Vec::with_capacity(k);
            for _ in 0..k {
                ops.push(native::decode_op(r.bytes("op", 64 << 20)?, attrs + n, styles)?);
            }
            Record::Action(Logged::Run { label, ops, merge, attrs: new })
        }
        2 => Record::Action(Logged::Undo),
        3 => Record::Action(Logged::Redo),
        5 => Record::Saved,
        _ => return Err(WireError::Invalid("record")),
    };
    if !r.is_empty() {
        return Err(WireError::Invalid("trailing bytes"));
    }
    Ok(rec)
}

/// A document's autosave folder.
pub struct Autosave {
    dir: PathBuf,
    journal: JournalWriter<File>,
    unsynced: usize,
    _lock: File,
}

impl Autosave {
    /// Starts autosaving `base` (a native file's bytes) into `dir` (created).
    pub fn start(dir: &Path, base: &[u8], name: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let l = lock(dir)?;
        let old = generation(dir).ok();
        let g = old.map_or(1, |g| g + 1);
        let tmp = dir.join(".base.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(base)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, base_path(dir, g))?;
        let j = OpenOptions::new().create(true).write(true).truncate(true).open(dir.join(format!("journal.{g}")))?;
        j.sync_all()?;
        fs::write(dir.join(META), name.as_bytes())?;
        let cur = dir.join(".current.tmp");
        {
            let mut c = File::create(&cur)?;
            c.write_all(g.to_string().as_bytes())?;
            c.sync_all()?;
        }
        sync_dir(dir);
        fs::rename(&cur, dir.join(CURRENT))?;
        sync_dir(dir);
        if let Some(o) = old.filter(|&o| o != g) {
            let _ = fs::remove_file(base_path(dir, o));
            let _ = fs::remove_file(dir.join(format!("journal.{o}")));
        }
        Ok(Self { dir: dir.to_path_buf(), journal: JournalWriter::new(j), unsynced: 0, _lock: l })
    }

    fn resume(dir: &Path, l: File, g: u64, good: u64) -> io::Result<Self> {
        let mut j = OpenOptions::new().write(true).open(dir.join(format!("journal.{g}")))?;
        j.set_len(good)?;
        j.seek(io::SeekFrom::End(0))?;
        j.sync_all()?;
        Ok(Self { dir: dir.to_path_buf(), journal: JournalWriter::new(j), unsynced: 0, _lock: l })
    }

    fn append(&mut self, r: &Record) -> io::Result<()> {
        self.journal.append(&encode(r))?;
        self.unsynced += 1;
        Ok(())
    }

    /// Makes the records so far durable (every few seconds and on close).
    pub fn sync(&mut self) -> io::Result<()> {
        if self.unsynced > 0 {
            self.journal.flush()?;
            self.journal.get_ref().sync_data()?;
            self.unsynced = 0;
        }
        Ok(())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Autosave folders under `root` left by documents that did not close cleanly.
pub fn pending(root: &Path) -> Vec<(PathBuf, String)> {
    let Ok(rd) = fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(PathBuf, String)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(CURRENT).is_file() && lock(p).is_ok())
        .map(|p| {
            let name = fs::read_to_string(p.join(META)).unwrap_or_default();
            (p, name)
        })
        .collect();
    out.sort();
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovered {
    pub actions: usize,
    pub torn_bytes: usize,
}

/// Rebuilds the document an autosave folder describes.
pub fn recover(dir: &Path) -> io::Result<(Doc, Recovered)> {
    let l = lock(dir)?;
    let g = generation(dir)?;
    let base = fs::read(base_path(dir, g))?;
    let loaded = native::load(&base).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let name = fs::read_to_string(dir.join(META)).unwrap_or_else(|_| "Recovered".into());
    let mut doc = Doc::from_text_doc(loaded.doc, &name);
    let data = fs::read(dir.join(format!("journal.{g}"))).unwrap_or_default();
    let replay = journal::read(&data);
    let (mut actions, mut since_save, mut torn) = (0, 0, replay.torn_bytes);
    {
        let mut e = doc.ed.borrow_mut();
        for (i, rec) in replay.records.iter().enumerate() {
            match decode(rec, e.doc.attrs.len(), e.doc.styles.styles.len()) {
                Ok(Record::Saved) => since_save = 0,
                Ok(Record::Action(a)) => {
                    e.replay(a);
                    actions += 1;
                    since_save += 1;
                }
                Err(_) => {
                    torn += replay.records[i..].iter().map(|r| r.len() + 8).sum::<usize>();
                    break;
                }
            }
        }
        e.start_log();
    }
    doc.dirty = since_save > 0;
    doc.autosave = Some(Autosave::resume(dir, l, g, (data.len() - torn) as u64)?);
    Ok((doc, Recovered { actions, torn_bytes: torn }))
}

/// One open text document.
pub struct Doc {
    pub ed: Rc<RefCell<Editor>>,
    pub name: String,
    /// Changed since the last save.
    pub dirty: bool,
    pub status: String,
    autosave: Option<Autosave>,
}

impl Doc {
    pub fn new(name: &str) -> Self {
        Self::from_text_doc(TextDoc::new(), name)
    }

    pub fn from_text_doc(d: TextDoc, name: &str) -> Self {
        let mut e = Editor::new(d, fonts());
        e.history.limit = UNDO_LIMIT;
        Self { ed: Rc::new(RefCell::new(e)), name: name.into(), dirty: false, status: String::new(), autosave: None }
    }

    /// A native file.
    pub fn from_native(name: &str, data: &[u8]) -> Result<Self, native::LoadError> {
        Ok(Self::from_text_doc(native::load(data)?.doc, name))
    }

    /// Plain text: a paragraph per line (UTF-8, or Windows-1252 when it isn't).
    pub fn from_text(name: &str, data: &[u8]) -> Result<Self, String> {
        if data.len() > MAX_TEXT_FILE {
            return Err("the file is over 256 MiB".into());
        }
        let text = match std::str::from_utf8(data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data)) {
            Ok(s) => s.to_string(),
            Err(_) => data.iter().map(|&b| cp1252(b)).collect(),
        };
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let clean: String = line.chars().filter(|&c| !c.is_control() || c == '\t').collect();
            d.push_para(&clean, 0, vec![]);
        }
        Ok(Self::from_text_doc(d, name))
    }

    /// Starts the autosave journal in `dir`, from the document as it is.
    pub fn autosave_to(&mut self, dir: &Path) -> io::Result<()> {
        if let Some(a) = self.autosave.take() {
            a.remove()?;
        }
        let base = self.native_bytes();
        self.autosave = Some(Autosave::start(dir, &base, &self.name)?);
        self.ed.borrow_mut().start_log();
        Ok(())
    }

    pub fn autosave(&mut self) -> Option<&mut Autosave> {
        self.autosave.as_mut()
    }

    /// Journals what the editor did since the last call; true if anything.
    pub fn sync_log(&mut self) -> bool {
        let log = self.ed.borrow_mut().take_log();
        if log.is_empty() {
            return false;
        }
        for a in log {
            if let Some(s) = &mut self.autosave {
                if let Err(e) = s.append(&Record::Action(a)) {
                    self.status = format!("Autosave failed: {e}");
                }
            }
        }
        self.dirty = true;
        true
    }

    /// The user's file now holds the document.
    pub fn saved(&mut self) -> io::Result<()> {
        self.sync_log();
        if let Some(a) = &mut self.autosave {
            a.append(&Record::Saved)?;
            a.sync()?;
        }
        self.dirty = false;
        Ok(())
    }

    /// A clean close: the autosave folder goes.
    pub fn close(mut self) -> io::Result<()> {
        match self.autosave.take() {
            Some(a) => a.remove(),
            None => Ok(()),
        }
    }

    pub fn native_bytes(&self) -> Vec<u8> {
        let e = self.ed.borrow();
        let cps: Vec<_> = if e.pages.done() { e.pages.pages().iter().map(|p| p.start).collect() } else { Vec::new() };
        native::save_to_vec(&e.doc, &cps)
    }

    /// The text, a line per paragraph.
    pub fn plain_text(&self) -> String {
        let e = self.ed.borrow();
        let mut s = String::new();
        for b in e.doc.blocks.iter() {
            if let Some(p) = b.as_para() {
                s.push_str(&p.text.replace('\u{2028}', "\n"));
                s.push('\n');
            }
        }
        s
    }
}

/// Windows-1252 to Unicode.
fn cp1252(b: u8) -> char {
    const HI: [u16; 32] = [0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD, 0x017D, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0xFFFD, 0x017E, 0x0178];
    match b {
        0x80..=0x9F => char::from_u32(HI[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}'),
        _ => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::event::{Key, Modifiers};
    use pergola_richtext::Pos;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("atrium-write-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    const NO: Modifiers = Modifiers { shift: false, ctrl: false, alt: false, meta: false };
    const CTRL: Modifiers = Modifiers { shift: false, ctrl: true, alt: false, meta: false };

    fn key(d: &mut Doc, k: Key, m: Modifiers, c: &str) {
        d.ed.borrow_mut().key(k, m, c, 0);
        d.sync_log();
    }

    #[test]
    fn a_crash_loses_nothing_typed() {
        let dir = tmp("crash");
        let mut d = Doc::from_text("notes.txt", b"first line\nsecond line\n").unwrap();
        d.autosave_to(&dir).unwrap();
        d.ed.borrow_mut().set_caret(Pos::new(1, 6), false);
        for c in "and more ".chars() {
            key(&mut d, Key::Char, NO, &c.to_string());
        }
        key(&mut d, Key::Enter, NO, "");
        d.ed.borrow_mut().set_caret(Pos::new(0, 0), false);
        d.ed.borrow_mut().set_caret(Pos::new(0, 5), true);
        key(&mut d, Key::Char, CTRL, "b");
        key(&mut d, Key::Char, CTRL, "z");
        key(&mut d, Key::Char, CTRL, "y");
        d.autosave().unwrap().sync().unwrap();
        let want = d.plain_text();
        let want_runs = d.ed.borrow().doc.para(0).unwrap().runs.clone();
        // The window dies here: no close.
        std::mem::forget(d);
        // A torn last record is cut off.
        let j = journal_path(&dir).unwrap();
        let mut f = OpenOptions::new().append(true).open(&j).unwrap();
        f.write_all(&[9, 0, 0, 0, 1, 2]).unwrap();
        drop(f);
        // The lock is still held by the forgotten document in this process;
        // a new process would get it. Release it as the exit would.
        let (r, rec) = {
            // flock locks belong to the open file description: open the lock
            // file again and unlock that holder by closing its descriptor is
            // not possible here, so test recovery through a copy.
            let copy = tmp("crash-copy");
            fs::create_dir_all(&copy).unwrap();
            for e in fs::read_dir(&dir).unwrap().flatten() {
                if e.file_name() != LOCK {
                    fs::copy(e.path(), copy.join(e.file_name())).unwrap();
                }
            }
            recover(&copy).unwrap()
        };
        assert_eq!(r.plain_text(), want);
        assert_eq!(r.ed.borrow().doc.para(0).unwrap().runs.len(), want_runs.len());
        assert!(r.dirty);
        assert_eq!(rec.torn_bytes, 6);
        assert!(rec.actions >= 4);
        // Undo runs back to the file as opened.
        while r.ed.borrow_mut().undo() {}
        assert_eq!(r.plain_text(), "first line\nsecond line\n\n");
        r.close().unwrap();
    }

    #[test]
    fn saving_marks_clean_and_native_round_trips() {
        let dir = tmp("save");
        let mut d = Doc::new("Untitled");
        d.autosave_to(&dir).unwrap();
        key(&mut d, Key::Char, NO, "H");
        key(&mut d, Key::Char, NO, "i");
        assert!(d.dirty);
        let bytes = d.native_bytes();
        d.saved().unwrap();
        assert!(!d.dirty);
        let back = Doc::from_native("x.owrite", &bytes).unwrap();
        assert_eq!(back.plain_text(), "Hi\n");
        d.close().unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn text_files_in_utf8_and_windows_1252() {
        let d = Doc::from_text("a.txt", "naïve\r\ncafé".as_bytes()).unwrap();
        assert_eq!(d.plain_text(), "naïve\ncafé\n");
        let d = Doc::from_text("b.txt", b"caf\xe9 \x93quoted\x94").unwrap();
        assert_eq!(d.plain_text(), "café “quoted”\n");
    }
}
