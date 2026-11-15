//! One open deck: the deck, its undo history, and its autosave (officina.md
//! §4.4), as Sheets and Write keep theirs.
//!
//! **Autosave.** Each document has a private folder holding `base.<g>.oslides`
//! (the deck as opened) and `journal.<g>`, every action since, appended as it
//! happens. A clean close removes the folder; after a crash [`recover`] loads
//! the base and replays the journal through the same history.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use officina_core::history::{History, Op};
use officina_core::journal::{self, JournalWriter};
use officina_core::wire::{Reader, WireError, Writer};
use officina_slides::model::{Deck, MediaId, SlideId};
use officina_slides::native;
use officina_slides::ops::DeckOp;

pub const UNDO_LIMIT: usize = 1000;

const CURRENT: &str = "current";
const META: &str = "name";
const LOCK: &str = "lock";

fn base_path(dir: &Path, g: u64) -> PathBuf {
    dir.join(format!("base.{g}.oslides"))
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

/// One record: one user action. A run's ops are deck journal records
/// ([`native::encode_record`]), each taken right after its op applied.
#[derive(Debug, Clone, PartialEq)]
enum Record {
    Run { label: String, ops: Vec<Vec<u8>> },
    /// More ops for the last action (a run of typing).
    Extend { ops: Vec<Vec<u8>> },
    Undo,
    Redo,
    Saved,
    /// A picture or video added to the deck's media, under its id.
    Media { id: u32, mime: String, bytes: Vec<u8> },
}

/// The largest media item a journal record holds.
pub const MAX_MEDIA: usize = 60 << 20;

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        let ops = |w: &mut Writer, ops: &[Vec<u8>]| {
            w.var(ops.len() as u64);
            for o in ops {
                w.bytes(o);
            }
        };
        match self {
            Record::Run { label, ops: o } => {
                w.u8(1);
                w.str(label);
                ops(&mut w, o);
            }
            Record::Extend { ops: o } => {
                w.u8(5);
                ops(&mut w, o);
            }
            Record::Undo => w.u8(2),
            Record::Redo => w.u8(3),
            Record::Saved => w.u8(4),
            Record::Media { id, mime, bytes } => {
                w.u8(6);
                w.var(*id as u64);
                w.str(mime);
                w.bytes(bytes);
            }
        }
        w.into_bytes()
    }

    fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(b);
        let ops = |r: &mut Reader| -> Result<Vec<Vec<u8>>, WireError> {
            let n = r.count("ops", 1 << 20, 1)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(r.bytes("op", 64 << 20)?.to_vec());
            }
            Ok(v)
        };
        let rec = match r.u8()? {
            1 => {
                let label = r.str("label", 256)?.to_string();
                Record::Run { label, ops: ops(&mut r)? }
            }
            5 => Record::Extend { ops: ops(&mut r)? },
            2 => Record::Undo,
            3 => Record::Redo,
            4 => Record::Saved,
            6 => {
                let id = u32::try_from(r.var()?).map_err(|_| WireError::Invalid("media id"))?;
                let mime = r.str("mime", 256)?.to_string();
                Record::Media { id, mime, bytes: r.bytes("media", MAX_MEDIA as u64)?.to_vec() }
            }
            _ => return Err(WireError::Invalid("record")),
        };
        if !r.is_empty() {
            return Err(WireError::Invalid("trailing bytes"));
        }
        Ok(rec)
    }
}

/// A document's autosave folder.
pub struct Autosave {
    dir: PathBuf,
    journal: JournalWriter<File>,
    unsynced: usize,
    _lock: File,
}

impl Autosave {
    pub fn start(dir: &Path, deck: &Deck, name: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let l = lock(dir)?;
        let g = generation(dir).map(|g| g + 1).unwrap_or(1);
        let old = generation(dir).ok();
        let tmp = dir.join(".base.tmp");
        let f = File::create(&tmp)?;
        let f = native::save(deck, io::BufWriter::new(f))?.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
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
        self.journal.append(&r.encode())?;
        self.unsynced += 1;
        Ok(())
    }

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

/// Autosave folders under `root` left by decks that did not close cleanly.
pub fn pending(root: &Path) -> Vec<(PathBuf, String)> {
    let Ok(rd) = fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(PathBuf, String)> = rd.flatten().map(|e| e.path()).filter(|p| p.join(CURRENT).is_file() && lock(p).is_ok()).map(|p| (p.clone(), fs::read_to_string(p.join(META)).unwrap_or_default())).collect();
    out.sort();
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovered {
    pub actions: usize,
    pub torn_bytes: usize,
}

/// Rebuilds the deck an autosave folder describes.
pub fn recover(dir: &Path) -> io::Result<(Doc, Recovered)> {
    let l = lock(dir)?;
    let g = generation(dir)?;
    let base = fs::read(base_path(dir, g))?;
    let deck = native::load(Arc::new(base)).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let name = fs::read_to_string(dir.join(META)).unwrap_or_else(|_| "Recovered".into());
    let mut doc = Doc::from_deck(deck, &name);
    let data = fs::read(dir.join(format!("journal.{g}"))).unwrap_or_default();
    let replay = journal::read(&data);
    let (mut actions, mut since_save, mut torn) = (0, 0, replay.torn_bytes);
    for (i, rec) in replay.records.iter().enumerate() {
        match Record::decode(rec).ok().filter(|r| doc.replay(r.clone())) {
            Some(Record::Saved) => since_save = 0,
            Some(_) => {
                actions += 1;
                since_save += 1;
            }
            None => {
                torn += replay.records[i..].iter().map(|r| r.len() + 8).sum::<usize>();
                break;
            }
        }
    }
    doc.dirty = since_save > 0;
    doc.autosave = Some(Autosave::resume(dir, l, g, (data.len() - torn) as u64)?);
    Ok((doc, Recovered { actions, torn_bytes: torn }))
}

/// One open deck.
pub struct Doc {
    pub deck: Deck,
    pub history: History<DeckOp>,
    pub name: String,
    pub dirty: bool,
    /// The slide on screen.
    pub current: usize,
    autosave: Option<Autosave>,
    pub status: String,
    /// The fidelity notes of the import that made this document.
    pub notes: Vec<String>,
    /// Bumped on every change (thumbnails and caches key on it with the slide).
    pub generation: u64,
    /// The slides each change touched, for the thumbnail cache.
    pub touched: Vec<SlideId>,
    /// The run of typing the last action is: (label, slide, shape).
    typing: Option<(String, SlideId, u32)>,
}

impl Doc {
    /// A new deck: a title slide.
    pub fn new(name: &str) -> Self {
        let mut deck = Deck::new();
        if let Some(l) = deck.layout_of_kind("title").map(|l| l.id) {
            let s = officina_slides::ops::new_slide(&mut deck, l);
            deck.push_slide(s);
        }
        Self::from_deck(deck, name)
    }

    pub fn from_deck(deck: Deck, name: &str) -> Self {
        let mut history = History::new();
        history.limit = UNDO_LIMIT;
        Doc { deck, history, name: name.into(), dirty: false, current: 0, autosave: None, status: String::new(), notes: Vec::new(), generation: 0, touched: Vec::new(), typing: None }
    }

    pub fn from_native(name: &str, data: Vec<u8>) -> Result<Self, String> {
        let deck = native::load(Arc::new(data)).map_err(|e| e.to_string())?;
        Ok(Self::from_deck(deck, name))
    }

    pub fn autosave_to(&mut self, dir: &Path) -> io::Result<()> {
        if let Some(a) = self.autosave.take() {
            a.remove()?;
        }
        self.autosave = Some(Autosave::start(dir, &self.deck, &self.name)?);
        Ok(())
    }

    pub fn autosave(&mut self) -> Option<&mut Autosave> {
        self.autosave.as_mut()
    }

    fn journal(&mut self, r: &Record) {
        if let Some(a) = &mut self.autosave {
            if let Err(e) = a.append(r) {
                self.status = format!("Autosave failed: {e}");
            }
        }
    }

    fn note_touch(&mut self, op: &DeckOp) {
        use officina_slides::ops::Target;
        self.generation += 1;
        match op {
            DeckOp::SetSlide { id, .. } | DeckOp::MoveSlide { id, .. } | DeckOp::RemoveSlide { id } => self.touched.push(*id),
            DeckOp::InsertSlide { slide, .. } => self.touched.push(slide.id),
            DeckOp::InsertShape { target: Target::Slide(id), .. } | DeckOp::RemoveShape { target: Target::Slide(id), .. } | DeckOp::SetShape { target: Target::Slide(id), .. } | DeckOp::MoveShape { target: Target::Slide(id), .. } | DeckOp::Text { target: Target::Slide(id), .. } => self.touched.push(*id),
            // Masters, layouts, the theme: every slide may change.
            _ => self.touched.push(0),
        }
    }

    /// Applies `ops` one by one, each journaled as it left the deck.
    fn apply(&mut self, label: &str, ops: Vec<DeckOp>, extend: bool) -> Vec<Vec<u8>> {
        let mut recs = Vec::with_capacity(ops.len());
        for (i, op) in ops.into_iter().enumerate() {
            self.note_touch(&op);
            let rec = {
                let probe = op.clone();
                if i == 0 && !extend {
                    self.history.run(&mut self.deck, label, vec![op]);
                } else {
                    self.history.extend_last(&mut self.deck, label, vec![op]);
                }
                native::encode_record(&self.deck, &probe)
            };
            recs.push(rec);
        }
        recs
    }

    /// Replays a journal record; false if it doesn't fit (a damaged journal).
    fn replay(&mut self, r: Record) -> bool {
        match r {
            Record::Run { label, ops } => self.replay_ops(&label, ops, false),
            Record::Extend { ops } => self.replay_ops("", ops, true),
            Record::Undo => {
                self.history.undo(&mut self.deck);
                self.generation += 1;
                self.touched.push(0);
                true
            }
            Record::Redo => {
                self.history.redo(&mut self.deck);
                self.generation += 1;
                self.touched.push(0);
                true
            }
            Record::Saved => true,
            Record::Media { id, mime, bytes } => {
                self.deck.put_media(id, &mime, &bytes);
                true
            }
        }
    }

    fn replay_ops(&mut self, label: &str, ops: Vec<Vec<u8>>, extend: bool) -> bool {
        for (i, b) in ops.iter().enumerate() {
            let Ok(op) = native::decode_record(&mut self.deck, b) else { return false };
            self.note_touch(&op);
            if i == 0 && !extend {
                self.history.run(&mut self.deck, label, vec![op]);
            } else {
                self.history.extend_last(&mut self.deck, label, vec![op]);
            }
        }
        true
    }

    /// Adds media to the deck (journaled, so a crash keeps it) and returns its
    /// id; the same bytes again give the same id.
    pub fn add_media(&mut self, mime: &str, bytes: &[u8]) -> Result<MediaId, String> {
        if bytes.len() > MAX_MEDIA {
            return Err(format!("It is over {} MB, more than a slide takes.", MAX_MEDIA >> 20));
        }
        let before = self.deck.media_count();
        let id = self.deck.add_media(mime, bytes);
        if self.deck.media_count() != before {
            self.journal(&Record::Media { id, mime: mime.into(), bytes: bytes.to_vec() });
        }
        Ok(id)
    }

    /// Runs `ops` as one undo step: applied and journaled.
    pub fn run(&mut self, label: &str, ops: Vec<DeckOp>) {
        if ops.is_empty() {
            return;
        }
        self.typing = None;
        let recs = self.apply(label, ops, false);
        self.journal(&Record::Run { label: label.into(), ops: recs });
        self.dirty = true;
    }

    /// Runs `ops` as part of the last undo step when that is the same kind
    /// of typing (`label`) in the same frame, else as a new one: a run of
    /// typing undoes as one step.
    pub fn typing(&mut self, label: &str, slide: SlideId, shape: u32, ops: Vec<DeckOp>) {
        if ops.is_empty() {
            return;
        }
        let key = (label.to_string(), slide, shape);
        if self.typing.as_ref() != Some(&key) || !self.history.can_undo() {
            self.run(label, ops);
            self.typing = Some(key);
            return;
        }
        let recs = self.apply(label, ops, true);
        self.journal(&Record::Extend { ops: recs });
        self.dirty = true;
    }

    /// The next change starts a new undo step.
    pub fn break_typing(&mut self) {
        self.typing = None;
    }

    /// Applies an op outside undo (never journaled; for views that try an op
    /// and take it back).
    pub fn probe(&mut self, op: DeckOp) -> DeckOp {
        op.apply(&mut self.deck)
    }

    pub fn undo(&mut self) -> bool {
        if !self.history.can_undo() {
            return false;
        }
        self.typing = None;
        self.journal(&Record::Undo);
        self.replay(Record::Undo);
        self.dirty = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        if !self.history.can_redo() {
            return false;
        }
        self.typing = None;
        self.journal(&Record::Redo);
        self.replay(Record::Redo);
        self.dirty = true;
        true
    }

    pub fn write_native<W: Write>(&self, out: W) -> io::Result<W> {
        native::save(&self.deck, out)
    }

    pub fn saved(&mut self) -> io::Result<()> {
        self.dirty = false;
        self.journal(&Record::Saved);
        if let Some(a) = &mut self.autosave {
            a.sync()?;
        }
        Ok(())
    }

    pub fn close(mut self) -> io::Result<()> {
        match self.autosave.take() {
            Some(a) => a.remove(),
            None => Ok(()),
        }
    }

    /// The id of the slide on screen.
    pub fn current_id(&self) -> Option<SlideId> {
        self.deck.slide_ids().nth(self.current)
    }

    /// Takes the slides changed since the last call (0: all of them).
    pub fn take_touched(&mut self) -> Vec<SlideId> {
        std::mem::take(&mut self.touched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_slides::model::*;
    use officina_slides::ops::{new_slide, Target};
    use officina_text::ops::{Fragment, TextOp};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atrium-slides-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn content(d: &mut Deck) -> Vec<Slide> {
        (0..d.slide_count()).map(|i| d.slide(i).unwrap().clone()).collect()
    }

    #[test]
    fn a_crash_comes_back_with_its_undo_steps() {
        let dir = tmp("crash");
        let mut doc = Doc::new("Talk");
        doc.autosave_to(&dir).unwrap();
        let obj = doc.deck.layout_of_kind("obj").unwrap().id;
        let s = new_slide(&mut doc.deck, obj);
        let sid = s.id;
        doc.run("New Slide", vec![DeckOp::InsertSlide { at: 1, slide: Box::new(s) }]);
        let shape = doc.deck.slide(1).unwrap().shapes[0].id;
        for ch in ["H", "i", "!"] {
            let at = doc.deck.slide(1).unwrap().shapes[0].text().unwrap().doc.para(0).unwrap().text.len() as u32;
            doc.typing("Typing", sid, shape, vec![DeckOp::Text { target: Target::Slide(sid), shape, op: TextOp::Splice { block: 0, at, del: 0, ins: Fragment { text: ch.into(), runs: vec![(0, 0)] } } }]);
        }
        let id2 = doc.deck.new_id();
        doc.run("Rectangle", vec![DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 9, shape: Box::new(Shape::rect(id2, Rect::new(0, 0, 10, 10), Fill::Solid(Color::rgb(0xff0000)))) }]);
        doc.undo();
        // A picture: its bytes are journaled before the shape that shows it.
        let media = doc.add_media("image/png", b"not really a png, but bytes").unwrap();
        assert_eq!(doc.add_media("image/png", b"not really a png, but bytes").unwrap(), media);
        let pid = doc.deck.new_id();
        doc.run("Insert Picture", vec![DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 0, shape: Box::new(Shape::new(pid, Rect::new(0, 0, 10, 10), ShapeKind::Picture { media, crop: [0; 4], line: None })) }]);
        doc.autosave().unwrap().sync().unwrap();
        let want = content(&mut doc.deck);
        drop(doc); // a crash: the folder stays
        let (mut back, rec) = recover(&dir).unwrap();
        assert_eq!(rec.torn_bytes, 0);
        assert!(back.dirty);
        assert!(content(&mut back.deck) == want);
        assert_eq!(&back.deck.media_bytes(media).unwrap()[..], &b"not really a png, but bytes"[..]);
        assert_eq!(back.deck.slide(1).unwrap().shapes[1].text().unwrap().text(), "Hi!");
        // The undo steps came back too: undo and redo the picture, then undo
        // it and the typing (one step).
        assert!(back.undo());
        assert!(back.redo());
        assert_eq!(back.deck.slide(1).unwrap().shapes.len(), want[1].shapes.len());
        back.undo();
        back.undo();
        assert_eq!(back.deck.slide(1).unwrap().shapes[0].text().unwrap().text(), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
