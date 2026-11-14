//! One open spreadsheet: the workbook, its undo history, and its autosave (officina.md
//! §4.4).
//!
//! **Autosave.** Each document has a private folder. It holds `base.osheet`, the
//! workbook as it was opened or last saved (the native file), and `journal`, every
//! action since, appended as it happens (`officina_core::journal`: length and CRC per
//! record, so a crash mid-write loses at most the record being written). A clean close
//! removes the folder. After a crash the folder is still there: [`recover`] loads the
//! base and replays the journal through the same history, so the recovered document is
//! the one on screen at the crash, undo steps included. Nothing reaches the user's own
//! file without Save (D9).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};

use officina_core::history::History;
use officina_core::journal::{self, JournalWriter};
use officina_core::wire::{Reader, WireError, Writer};
use officina_grid::addr::{CellAddr, Rect};
use officina_grid::csv::{self, CsvOptions, CsvStats};
use officina_grid::native;
use officina_grid::ops::GridOp;
use officina_grid::{Input, Workbook};

/// Undo steps kept.
pub const UNDO_LIMIT: usize = 1000;
/// Cells one paste may set (a hostile clipboard can't make a billion ops).
pub const MAX_PASTE_CELLS: usize = 4_000_000;

/// The folder's generation in force: `base.<g>.osheet` plus `journal.<g>`. A new
/// base starts a new generation and switches with one rename, so a crash never pairs a
/// new base with an old journal (which would replay actions twice).
///
/// A base is written only when a document starts autosaving (new, opened). Saving
/// appends a `Saved` mark instead of a new base: the journal from the base is what
/// rebuilds the undo history, so an undo after a save, or after a recovery, replays
/// exactly as it ran.
const CURRENT: &str = "current";
const META: &str = "name";
/// Held (flock) while a window owns the folder, so a second window never takes over a
/// live document's autosave as if it had crashed.
const LOCK: &str = "lock";

fn base_path(dir: &Path, g: u64) -> PathBuf {
    dir.join(format!("base.{g}.osheet"))
}

/// The journal of the generation in force (for tests and tools).
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

/// One record of the journal: one user action.
#[derive(Debug, Clone, PartialEq)]
enum Record {
    Run { label: String, ops: Vec<GridOp> },
    Undo,
    Redo,
    ColWidth { sheet: u16, col: u32, width: f64 },
    /// The user's file now holds the document as it is here.
    Saved,
    /// The file's macros (kept inert) are dropped; the next save writes none.
    RemoveMacros,
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Record::Run { label, ops } => {
                w.u8(1);
                w.str(label);
                w.var(ops.len() as u64);
                for op in ops {
                    w.bytes(&op.encode());
                }
            }
            Record::Undo => w.u8(2),
            Record::Redo => w.u8(3),
            Record::ColWidth { sheet, col, width } => {
                w.u8(4);
                w.u16(*sheet);
                w.var(*col as u64);
                w.f64(*width);
            }
            Record::Saved => w.u8(5),
            Record::RemoveMacros => w.u8(6),
        }
        w.into_bytes()
    }

    fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(b);
        let rec = match r.u8()? {
            1 => {
                let label = r.str("label", 256)?.to_string();
                let n = r.count("ops", 1 << 20, 1)?;
                let mut ops = Vec::with_capacity(n);
                for _ in 0..n {
                    ops.push(GridOp::decode(r.bytes("op", 64 << 20)?)?);
                }
                Record::Run { label, ops }
            }
            2 => Record::Undo,
            3 => Record::Redo,
            4 => {
                let sheet = r.u16()?;
                let col = r.len("column", officina_grid::addr::MAX_COLS as u64 - 1)? as u32;
                let width = r.f64()?;
                if !(width.is_finite() && width > 0.0 && width <= 8192.0) {
                    return Err(WireError::Invalid("width"));
                }
                Record::ColWidth { sheet, col, width }
            }
            5 => Record::Saved,
            6 => Record::RemoveMacros,
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
    /// Records appended since the last [`Autosave::sync`].
    unsynced: usize,
    _lock: File,
}

impl Autosave {
    /// Starts autosaving `wb` into `dir` (created). Fails if another window holds it.
    pub fn start(dir: &Path, wb: &Workbook, name: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let l = lock(dir)?;
        let g = generation(dir).map(|g| g + 1).unwrap_or(1);
        Self::begin(dir, l, g, wb, name)
    }

    /// Writes generation `g` (base and empty journal) and switches to it.
    fn begin(dir: &Path, l: File, g: u64, wb: &Workbook, name: &str) -> io::Result<Self> {
        let old = generation(dir).ok();
        let tmp = dir.join(".base.tmp");
        let f = File::create(&tmp)?;
        let f = native::save(wb, io::BufWriter::new(f))?.into_inner().map_err(|e| e.into_error())?;
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
        // The switch.
        fs::rename(&cur, dir.join(CURRENT))?;
        sync_dir(dir);
        if let Some(o) = old.filter(|&o| o != g) {
            let _ = fs::remove_file(base_path(dir, o));
            let _ = fs::remove_file(dir.join(format!("journal.{o}")));
        }
        Ok(Self { dir: dir.to_path_buf(), journal: JournalWriter::new(j), unsynced: 0, _lock: l })
    }

    /// Goes on appending to generation `g`'s journal after its first `good` bytes
    /// (what a torn last record left is cut off first).
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

    /// Makes the records so far durable (every few seconds and on blur).
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

    /// A clean close: nothing to recover.
    pub fn remove(self) -> io::Result<()> {
        fs::remove_dir_all(&self.dir)
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Autosave folders under `root` left by documents that did not close cleanly, with
/// the name each was shown under.
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

/// What [`recover`] got back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovered {
    /// Actions replayed.
    pub actions: usize,
    /// Bytes at the journal's end that were a torn or damaged record (the crash).
    pub torn_bytes: usize,
}

/// Rebuilds the document an autosave folder describes: the base, then every journal
/// action replayed through the same history. The document keeps autosaving to the
/// same folder, from a new base.
pub fn recover(dir: &Path) -> io::Result<(Doc, Recovered)> {
    let l = lock(dir)?;
    let g = generation(dir)?;
    let base = fs::read(base_path(dir, g))?;
    let wb = native::load(&base).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let name = fs::read_to_string(dir.join(META)).unwrap_or_else(|_| "Recovered".into());
    let mut doc = Doc::from_workbook(wb, &name);
    let data = fs::read(dir.join(format!("journal.{g}"))).unwrap_or_default();
    let replay = journal::read(&data);
    let mut actions = 0;
    let mut since_save = 0;
    let mut torn = replay.torn_bytes;
    for (i, rec) in replay.records.iter().enumerate() {
        match Record::decode(rec) {
            Ok(Record::Saved) => since_save = 0,
            Ok(r) => {
                doc.replay(r);
                actions += 1;
                since_save += 1;
            }
            Err(_) => {
                // A record that passed its CRC but doesn't decode: stop there.
                torn += replay.records[i..].iter().map(|r| r.len() + 8).sum::<usize>();
                break;
            }
        }
    }
    doc.wb.recalc();
    doc.dirty = since_save > 0;
    doc.autosave = Some(Autosave::resume(dir, l, g, (data.len() - torn) as u64)?);
    Ok((doc, Recovered { actions, torn_bytes: torn }))
}

/// One open spreadsheet.
pub struct Doc {
    pub wb: Workbook,
    pub history: History<GridOp>,
    /// What the window shows as the document's name.
    pub name: String,
    /// Changed since the last save.
    pub dirty: bool,
    /// The sheet on screen.
    pub sheet: u16,
    autosave: Option<Autosave>,
    /// The last thing to tell the user (a formula error, a save).
    pub status: String,
    /// The separator CSV saves with: the one the file was read with.
    pub csv_sep: u8,
    /// The fidelity report of the import that made this document (docs/spec/officina.md
    /// §5.2), one line per note.
    pub notes: Vec<String>,
}

impl Doc {
    pub fn new(name: &str) -> Self {
        let mut wb = Workbook::new();
        wb.add_sheet("Sheet1").expect("first sheet");
        Self::from_workbook(wb, name)
    }

    pub fn from_workbook(wb: Workbook, name: &str) -> Self {
        Self { wb, history: { let mut h = History::new(); h.limit = UNDO_LIMIT; h }, name: name.into(), dirty: false, sheet: 0, autosave: None, status: String::new(), csv_sep: b',', notes: Vec::new() }
    }

    /// A CSV file's contents as a new one-sheet document. Formulas stay text.
    pub fn from_csv(name: &str, data: &[u8]) -> Result<(Self, CsvStats), csv::CsvError> {
        let mut doc = Self::new(name);
        let st = csv::read(&mut doc.wb, 0, data, CsvOptions::default())?;
        doc.csv_sep = st.separator;
        doc.wb.recalc();
        Ok((doc, st))
    }

    /// A native file.
    pub fn from_native(name: &str, data: &[u8]) -> Result<Self, native::LoadError> {
        let mut wb = native::load(data)?;
        wb.recalc();
        Ok(Self::from_workbook(wb, name))
    }

    /// Starts the autosave journal in `dir`.
    pub fn autosave_to(&mut self, dir: &Path) -> io::Result<()> {
        if let Some(a) = self.autosave.take() {
            a.remove()?;
        }
        self.autosave = Some(Autosave::start(dir, &self.wb, &self.name)?);
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

    fn replay(&mut self, r: Record) {
        match r {
            Record::Run { label, ops } => self.history.run(&mut self.wb, &label, ops),
            Record::Undo => {
                self.history.undo(&mut self.wb);
            }
            Record::Redo => {
                self.history.redo(&mut self.wb);
            }
            Record::Saved => {}
            Record::RemoveMacros => {
                self.wb.remove_macros();
            }
            Record::ColWidth { sheet, col, width } => {
                if (sheet as usize) < self.wb.sheet_count() {
                    self.wb.sheet_mut(sheet).col_sizes.set(col, width);
                }
            }
        }
    }

    /// Runs `ops` as one undo step: applied, journaled, recalculated.
    pub fn run(&mut self, label: &str, ops: Vec<GridOp>) {
        if ops.is_empty() {
            return;
        }
        let r = Record::Run { label: label.into(), ops };
        self.journal(&r);
        self.replay(r);
        self.wb.recalc();
        self.dirty = true;
    }

    /// The user typed `text` into a cell. A formula that doesn't parse is refused
    /// (the cell keeps what it had) and the reason goes to `status`.
    pub fn type_into(&mut self, row: u32, col: u32, text: &str) -> bool {
        let input = Input::typed(text);
        if let Input::Formula(f) = &input {
            if let Err(e) = self.wb.parse(f, self.sheet, row, col) {
                self.status = format!("That formula has a problem at character {}: {}", e.at + 1, e.msg);
                return false;
            }
        }
        if self.wb.input(CellAddr::new(self.sheet, row, col)) == input {
            return true;
        }
        self.status.clear();
        self.run("Typing", vec![GridOp::set(CellAddr::new(self.sheet, row, col), input)]);
        true
    }

    pub fn clear(&mut self, r0: u32, c0: u32, r1: u32, c1: u32) {
        self.run("Clear", vec![GridOp::clear(Rect { sheet: self.sheet, r0, c0, r1, c1 })]);
    }

    /// Pastes tab-separated text (a range from any spreadsheet) at (`row`, `col`).
    pub fn paste_tsv(&mut self, row: u32, col: u32, text: &str) -> bool {
        let text = text.strip_suffix('\n').unwrap_or(text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let rows: Vec<Vec<Input>> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).split('\t').map(Input::typed).collect()).collect();
        let cells: usize = rows.iter().map(Vec::len).sum();
        let w = rows.iter().map(Vec::len).max().unwrap_or(0) as u64;
        if cells > MAX_PASTE_CELLS || row as u64 + rows.len() as u64 > officina_grid::addr::MAX_ROWS as u64 || col as u64 + w > officina_grid::addr::MAX_COLS as u64 {
            self.status = "That is more than a paste can hold.".into();
            return false;
        }
        // Formulas that don't parse are pasted as text.
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                r.into_iter()
                    .enumerate()
                    .map(|(j, inp)| match inp {
                        Input::Formula(f) if self.wb.parse(&f, self.sheet, row + i as u32, col + j as u32).is_err() => Input::Value(officina_grid::Value::str(&f)),
                        x => x,
                    })
                    .collect()
            })
            .collect();
        self.run("Paste", vec![GridOp::block(self.sheet, row, col, rows)]);
        true
    }

    /// The range as tab-separated values, as other spreadsheets paste it.
    pub fn copy_tsv(&self, r0: u32, c0: u32, r1: u32, c1: u32) -> String {
        let (er, ec) = self.wb.extent(self.sheet);
        let (r1, c1) = (r1.min(er.saturating_sub(1)).max(r0), c1.min(ec.saturating_sub(1)).max(c0));
        let mut s = String::new();
        for r in r0..=r1 {
            for c in c0..=c1 {
                if c > c0 {
                    s.push('\t');
                }
                s.push_str(&self.wb.value(self.sheet, r, c).display().replace(['\t', '\n', '\r'], " "));
            }
            s.push('\n');
        }
        s
    }

    pub fn undo(&mut self) -> bool {
        if !self.history.can_undo() {
            return false;
        }
        self.journal(&Record::Undo);
        self.history.undo(&mut self.wb);
        self.wb.recalc();
        self.dirty = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        if !self.history.can_redo() {
            return false;
        }
        self.journal(&Record::Redo);
        self.history.redo(&mut self.wb);
        self.wb.recalc();
        self.dirty = true;
        true
    }

    pub fn set_col_width(&mut self, col: u32, width: f64) {
        let r = Record::ColWidth { sheet: self.sheet, col, width: width.clamp(1.0, 8192.0) };
        self.journal(&r);
        self.replay(r);
        self.dirty = true;
    }

    /// Drops the file's macros (not undoable: the file on disk still has them until
    /// the next save).
    pub fn remove_macros(&mut self) {
        if self.wb.macros().is_none() {
            return;
        }
        self.journal(&Record::RemoveMacros);
        self.replay(Record::RemoveMacros);
        self.dirty = true;
    }

    /// Writes the native file to `out`.
    pub fn write_native<W: Write>(&self, out: W) -> io::Result<W> {
        native::save(&self.wb, out)
    }

    /// Writes the current sheet as CSV to `out`.
    pub fn write_csv<W: Write>(&self, out: W) -> io::Result<W> {
        csv::write(&self.wb, self.sheet, self.csv_sep, out)
    }

    /// The user's file now holds this document: a new base, an empty journal.
    pub fn saved(&mut self) -> io::Result<()> {
        self.dirty = false;
        self.journal(&Record::Saved);
        if let Some(a) = &mut self.autosave {
            a.sync()?;
        }
        Ok(())
    }

    /// A clean close: the autosave folder goes.
    pub fn close(mut self) -> io::Result<()> {
        match self.autosave.take() {
            Some(a) => a.remove(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::Value;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atrium-sheets-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn v(doc: &Doc, r: u32, c: u32) -> Value {
        doc.wb.get(CellAddr::new(0, r, c))
    }

    #[test]
    fn records_round_trip() {
        for r in [
            Record::Run { label: "Typing".into(), ops: vec![GridOp::set(CellAddr::new(0, 1, 2), Input::Formula("=A1*2".into()))] },
            Record::Undo,
            Record::Redo,
            Record::ColWidth { sheet: 0, col: 3, width: 120.5 },
            Record::Saved,
            Record::RemoveMacros,
        ] {
            assert_eq!(Record::decode(&r.encode()).unwrap(), r);
        }
        assert!(Record::decode(&[9]).is_err());
        assert!(Record::decode(&[4, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0xf8, 0x7f]).is_err(), "NaN width");
    }

    #[test]
    fn typing_paste_undo_and_a_bad_formula() {
        let mut d = Doc::new("Budget");
        assert!(d.type_into(0, 0, "2"));
        assert!(d.type_into(1, 0, "=A1*21"));
        assert_eq!(v(&d, 1, 0), Value::Num(42.0));
        assert!(!d.type_into(2, 0, "=SUM(A1"), "refused");
        assert!(d.status.contains("problem"), "{}", d.status);
        assert_eq!(v(&d, 2, 0), Value::Empty);
        d.paste_tsv(0, 2, "1\t2\r\n=C1+D1\tx\n");
        assert_eq!(v(&d, 1, 2), Value::Num(3.0));
        assert_eq!(v(&d, 1, 3), Value::str("x"));
        assert_eq!(d.copy_tsv(0, 2, 1, 3), "1\t2\n3\tx\n");
        assert!(d.undo());
        assert_eq!(v(&d, 1, 2), Value::Empty);
        assert!(d.redo());
        assert_eq!(v(&d, 1, 2), Value::Num(3.0));
    }

    /// The O2 gate: a crash at any point loses at most the action being written.
    #[test]
    fn crash_recovery_restores_the_document_on_screen() {
        let dir = tmp("crash");
        let mut d = Doc::from_csv("data.csv", b"a,b\n1,2\n3,4\n").unwrap().0;
        d.autosave_to(&dir).unwrap();
        d.type_into(3, 0, "=SUM(A2:A3)");
        d.type_into(3, 1, "=SUM(B2:B3)");
        d.set_col_width(1, 140.0);
        d.paste_tsv(5, 0, "x\ty\n10\t20");
        d.undo();
        d.type_into(1, 0, "100");
        d.clear(2, 1, 2, 1);
        d.undo();
        d.redo();
        d.autosave().unwrap().sync().unwrap();
        let want: Vec<Value> = (0..8).flat_map(|r| (0..3).map(move |c| (r, c))).map(|(r, c)| v(&d, r, c)).collect();
        // The process dies here: no close.
        drop(d);

        let (mut back, rec) = recover(&dir).unwrap();
        assert_eq!(rec, Recovered { actions: 9, torn_bytes: 0 });
        let got: Vec<Value> = (0..8).flat_map(|r| (0..3).map(move |c| (r, c))).map(|(r, c)| v(&back, r, c)).collect();
        assert_eq!(got, want);
        assert_eq!(v(&back, 3, 0), Value::Num(103.0));
        assert_eq!(back.wb.sheet(0).col_sizes.size(1), 140.0);
        assert!(back.dirty, "recovered edits are unsaved");
        // The undo history came back too: undo the redo of the clear.
        assert!(back.undo());
        assert_eq!(v(&back, 2, 1), Value::Num(4.0));
        assert_eq!(pending(dir.parent().unwrap()).iter().filter(|(p, _)| *p == dir).count(), 0, "held by the recovered window");
        // It goes on journaling where it left off; a second crash restores the undo too.
        back.type_into(7, 2, "z");
        back.autosave().unwrap().sync().unwrap();
        drop(back);
        assert_eq!(pending(dir.parent().unwrap()).iter().filter(|(p, _)| *p == dir).count(), 1);
        let (back, rec) = recover(&dir).unwrap();
        assert_eq!(rec.actions, 11);
        assert_eq!(v(&back, 2, 1), Value::Num(4.0));
        assert_eq!(v(&back, 7, 2), Value::str("z"));
        back.close().unwrap();
        assert!(!dir.exists(), "a clean close leaves nothing to recover");
    }

    #[test]
    fn removing_macros_survives_a_crash() {
        use officina_grid::macros::*;
        let dir = tmp("macros");
        let mut wb = officina_grid::Workbook::new();
        wb.add_sheet("A").unwrap();
        wb.set_macros(Macros { family: MacroFamily::Ooxml, parts: vec![MacroPart { path: "xl/vbaProject.bin".into(), media_type: "application/vnd.ms-office.vbaProject".into(), data: vec![1, 2, 3].into() }], workbook_code: None, sheet_codes: vec![] }).unwrap();
        let mut d = Doc::from_native("m.osheet", &native::save_to_vec(&wb)).unwrap();
        assert!(d.wb.macros().is_some());
        d.autosave_to(&dir).unwrap();
        d.remove_macros();
        assert!(d.wb.macros().is_none() && d.dirty);
        d.autosave().unwrap().sync().unwrap();
        drop(d);
        let (back, rec) = recover(&dir).unwrap();
        assert_eq!(rec.actions, 1);
        assert!(back.wb.macros().is_none());
        back.close().unwrap();
    }

    #[test]
    fn a_torn_last_record_loses_only_that_action() {
        let dir = tmp("torn");
        let mut d = Doc::new("t");
        d.autosave_to(&dir).unwrap();
        for i in 0..20 {
            d.type_into(i, 0, &format!("{i}"));
        }
        d.autosave().unwrap().sync().unwrap();
        drop(d);
        let j = journal_path(&dir).unwrap();
        let bytes = fs::read(&j).unwrap();
        let full = bytes.len();
        let base = fs::read(base_path(&dir, generation(&dir).unwrap())).unwrap();
        // Every place the crash could have cut the last write.
        let last_len = journal::read(&bytes).records.last().unwrap().len() + 8;
        for cut in 1..last_len {
            fs::write(&j, &bytes[..full - cut]).unwrap();
            let (mut back, rec) = recover(&dir).unwrap();
            assert_eq!(rec.actions, 19, "cut {cut}");
            assert!(rec.torn_bytes > 0);
            assert_eq!(back.wb.get(CellAddr::new(0, 18, 0)), Value::Num(18.0));
            assert_eq!(back.wb.get(CellAddr::new(0, 19, 0)), Value::Empty);
            // The torn tail is cut off before anything new is appended.
            back.type_into(19, 1, "after");
            back.autosave().unwrap().sync().unwrap();
            drop(back);
            let (again, rec) = recover(&dir).unwrap();
            assert_eq!((rec.actions, rec.torn_bytes), (20, 0), "cut {cut}");
            assert_eq!(again.wb.get(CellAddr::new(0, 19, 1)), Value::str("after"));
            drop(again);
            assert_eq!(fs::read(base_path(&dir, generation(&dir).unwrap())).unwrap(), base);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_is_marked_and_undo_still_replays() {
        let dir = tmp("save");
        let mut d = Doc::new("s");
        d.autosave_to(&dir).unwrap();
        d.type_into(0, 0, "1");
        let mut file = Vec::new();
        file = d.write_native(std::mem::take(&mut file)).unwrap();
        d.saved().unwrap();
        assert!(!d.dirty);
        d.type_into(0, 1, "2");
        d.autosave().unwrap().sync().unwrap();
        drop(d);
        let (mut back, rec) = recover(&dir).unwrap();
        assert_eq!(rec.actions, 2);
        assert!(back.dirty, "a change after the save");
        assert_eq!(back.wb.get(CellAddr::new(0, 0, 0)), Value::Num(1.0));
        assert_eq!(back.wb.get(CellAddr::new(0, 0, 1)), Value::Num(2.0));
        // Undo goes back past the save, as it would have before the crash.
        assert!(back.undo() && back.undo());
        assert_eq!(back.wb.get(CellAddr::new(0, 0, 0)), Value::Empty);
        drop(back);
        let (back, _) = recover(&dir).unwrap();
        assert_eq!(back.wb.get(CellAddr::new(0, 0, 0)), Value::Empty);
        drop(back);
        let saved = Doc::from_native("s", &file).unwrap();
        assert_eq!(saved.wb.get(CellAddr::new(0, 0, 1)), Value::Empty);
        let _ = fs::remove_dir_all(&dir);
    }
}
