//! Files: what a name says a file is, and opening a foreign one through the converter
//! process.

use crate::doc::Doc;
use officina_convert::crypt::CryptError;
use officina_convert::launch::ConvertError;
use officina_convert::sealed::{self, Protection};

/// What a file is, by its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Native,
    Csv,
    Xlsx,
    /// `.xlsm`: an `.xlsx` that may carry macros (kept inert, never run).
    Xlsm,
    Ods,
    /// `.xls`, Excel 97–2003: read, never written.
    Xls,
}

pub fn format_of(name: &str) -> Format {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".osheet") {
        Format::Native
    } else if n.ends_with(".xlsx") {
        Format::Xlsx
    } else if n.ends_with(".xlsm") {
        Format::Xlsm
    } else if n.ends_with(".ods") {
        Format::Ods
    } else if n.ends_with(".xls") {
        Format::Xls
    } else {
        Format::Csv
    }
}

pub fn is_native(name: &str) -> bool {
    format_of(name) == Format::Native
}

/// Runs the converter process on `data` (docs/spec/officina.md §5.2): the foreign
/// file is parsed there, sandboxed, never here. Returns the native file and the
/// fidelity notes.
pub fn convert(kind: &str, data: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    officina_convert::launch::convert(kind, data, None).map_err(|e| e.to_string())
}

/// A file being read on its own thread ([`Loading::start`]): converted,
/// loaded and recalculated there, so the window keeps answering.
pub struct Loading {
    rx: std::sync::mpsc::Receiver<(Result<Doc, ConvertError>, Vec<u8>)>,
}

impl Loading {
    /// Starts reading `data` as [`load_with`] does.
    pub fn start(name: &str, data: Vec<u8>, password: Option<&str>) -> std::io::Result<Loading> {
        let (n, pw) = (name.to_string(), password.map(str::to_string));
        Self::spawn(move || {
            let r = load_with(&n, &data, pw.as_deref());
            (r, data)
        })
    }

    /// Restores the autosave folder `dir` (a document Sheets didn't close),
    /// as [`crate::doc::recover`] does, named `name` in the status line.
    pub fn restore(dir: std::path::PathBuf, name: &str) -> std::io::Result<Loading> {
        let name = name.to_string();
        Self::spawn(move || match crate::doc::recover(&dir) {
            Ok((mut d, rec)) => {
                d.status = format!("Restored “{name}” as it was before Sheets last stopped ({} changes since its last save).", rec.actions);
                (Ok(d), Vec::new())
            }
            Err(e) => (Err(ConvertError::Failed(format!("Couldn't restore “{name}”: {e}"))), Vec::new()),
        })
    }

    fn spawn(f: impl FnOnce() -> (Result<Doc, ConvertError>, Vec<u8>) + Send + 'static) -> std::io::Result<Loading> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("open".into()).stack_size(16 << 20).spawn(move || {
            let _ = tx.send(f());
        })?;
        Ok(Loading { rx })
    }

    /// The document (and the bytes back, for a password retry) once read;
    /// None while still reading.
    pub fn poll(&self) -> Option<(Result<Doc, ConvertError>, Vec<u8>)> {
        match self.rx.try_recv() {
            Ok(x) => Some(x),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((Err(ConvertError::Failed("reading it stopped".into())), Vec::new())),
        }
    }
}

/// A document from a file's bytes, by its name's extension.
pub fn load(name: &str, data: &[u8]) -> Result<Doc, String> {
    load_with(name, data, None).map_err(|e| match e {
        ConvertError::Failed(w) => w,
        e => format!("“{name}” couldn't be opened: {e}."),
    })
}

/// [`load`] with the password of a protected file. `NeedsPassword` and
/// `WrongPassword` say to ask (again); anything else is in words.
pub fn load_with(name: &str, data: &[u8], password: Option<&str>) -> Result<Doc, ConvertError> {
    let failed = ConvertError::Failed;
    if is_native(name) && sealed::is_sealed(data) {
        let Some(pw) = password else { return Err(ConvertError::NeedsPassword) };
        let (plain, sealer) = sealed::unseal(data, pw).map_err(|e| match e {
            CryptError::WrongPassword => ConvertError::WrongPassword,
            e => failed(format!("“{name}” couldn't be opened: {e}")),
        })?;
        let mut d = Doc::from_native(name, &plain).map_err(|e| failed(format!("“{name}” isn't a sheet this app can read: {e}")))?;
        d.protection = Some(Protection { password: pw.to_string(), sealer });
        d.status = format!("Opened “{name}”, protected with a password.");
        return Ok(d);
    }
    if is_native(name) {
        return Doc::from_native(name, data).map_err(|e| failed(format!("“{name}” isn't a sheet this app can read: {e}")));
    }
    let foreign = match format_of(name) {
        Format::Xlsx | Format::Xlsm => Some("xlsx"),
        Format::Ods => Some("ods"),
        Format::Xls => Some("xls"),
        _ => None,
    };
    if let Some(kind) = foreign {
        let (native, notes) = officina_convert::launch::convert(kind, data, password).map_err(|e| match e {
            ConvertError::Failed(w) => failed(format!("“{name}” couldn't be opened: {w}")),
            e => e,
        })?;
        let mut d = Doc::from_native(name, &native).map_err(|e| failed(format!("“{name}”: the converter's output didn't load: {e}")))?;
        if let Some(pw) = password {
            d.protection = Some(Protection::new(pw));
        }
        d.status = match notes.len() {
            0 => format!("Opened “{name}”."),
            1 => format!("Opened “{name}”. {}", notes[0]),
            n => format!("Opened “{name}” with {n} notes. {} …", notes[0]),
        };
        for n in &notes {
            eprintln!("atrium-sheets: {name}: {n}");
        }
        d.notes = notes;
        return Ok(d);
    }
    let (mut d, st) = Doc::from_csv(name, data).map_err(|e| failed(format!("“{name}” couldn't be read as CSV: {e:?}")))?;
    if st.latin {
        d.notes.push("Read as Windows-1252 (not UTF-8).".into());
    }
    if st.cut > 0 {
        d.notes.push(format!("{} over-long cells cut.", st.cut));
    }
    d.status = format!("Opened “{name}”: {} rows × {} columns.", st.rows, st.cols);
    for note in &d.notes {
        d.status.push(' ');
        d.status.push_str(note);
    }
    Ok(d)
}


/// The name without its extension.
pub fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_slow_file_is_read_while_the_caller_carries_on() {
        // Volatile and slow: every row walks the column again on open.
        let rows = 20_000u32;
        let mut d = Doc::new("slow.osheet");
        let a: Vec<f64> = (0..rows).map(|r| (r % 97) as f64).collect();
        d.wb.set_numbers(0, 0, 0, &a);
        d.wb.fill_formula(0, 1, 0, rows - 1, &format!("=SUMIF($A$1:$A${rows},\">\"&A1)+RAND()*0")).unwrap();
        d.wb.recalc();
        let want = d.wb.value(0, 7, 1);
        let bytes = d.stored_native().unwrap();
        let t = Instant::now();
        let l = Loading::start("slow.osheet", bytes, None).unwrap();
        assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
        let mut polls = 0;
        let (r, _) = loop {
            let p = Instant::now();
            if let Some(x) = l.poll() {
                break x;
            }
            assert!(p.elapsed() < Duration::from_millis(5));
            polls += 1;
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(polls > 3, "the read took {polls} polls");
        assert_eq!(r.unwrap().wb.value(0, 7, 1), want);
        let l = Loading::start("bad.osheet", b"not a sheet".to_vec(), None).unwrap();
        let (r, back) = loop {
            if let Some(x) = l.poll() {
                break x;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(r, Err(ConvertError::Failed(_))));
        assert_eq!(back, b"not a sheet");
    }

    #[test]
    fn an_autosave_is_restored_on_its_own_thread() {
        let dir = std::env::temp_dir().join(format!("atrium-sheets-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut d = Doc::new("Book");
        d.autosave_to(&dir).unwrap();
        d.type_into(0, 0, "21");
        d.type_into(0, 1, "=A1*2");
        d.autosave().unwrap().sync().unwrap();
        drop(d);
        let l = Loading::restore(dir.clone(), "Book").unwrap();
        let (r, _) = loop {
            if let Some(x) = l.poll() {
                break x;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let back = r.unwrap();
        assert_eq!(back.wb.value(0, 0, 1), officina_grid::Value::Num(42.0));
        assert!(back.status.starts_with("Restored “Book”"), "{}", back.status);
        drop(back);
        let l = Loading::restore(dir.join("nothing"), "Gone").unwrap();
        let (r, _) = loop {
            if let Some(x) = l.poll() {
                break x;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(r, Err(ConvertError::Failed(e)) if e.starts_with("Couldn't restore “Gone”")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
