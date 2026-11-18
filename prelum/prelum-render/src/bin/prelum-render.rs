//! atrium-prelum-render: one job's conversion (printing.md §6.1).
//!
//! ```text
//! atrium-prelum-render --format pdf|pwg|urf [--fallback pwg|urf] [--dpi N]
//!     [--media KEYWORD] [--media-type KEYWORD] [--pages 1-3,5] [--per-sheet 1|2|4]
//!     [--color color|gray] [--duplex simplex|long|short] [--sheet-back KEYWORD]
//!     [--scale fit|shrink|PERCENT] [--quality 3|4|5]
//! ```
//!
//! stdin is the job's PDF (a file, read with pread), stdout a socket to
//! prelumd carrying `prelum_proto::render` frames, stderr the log. Started
//! by prelumd through jaild with no network and no filesystem; on FreeBSD
//! it enters Capsicum before it reads a byte of the document.
//!
//! `--format preview --first N --count C --longest PX` draws small images
//! of sheets N.. for the print sheet (§5.2) instead of printing anything.
//!
//! `--format pdf` writes the rewritten PDF (§6.2 1.) when the document
//! allows it, else the `--fallback` raster: the first frame says which.

use std::cell::RefCell;
use std::io::{self, Write};
use std::os::fd::FromRawFd;
use std::rc::Rc;
use std::time::Duration;

use prelum_ipp_codec::ops::Format;
use prelum_proto::frame::{self, DATA_CHUNK};
use prelum_proto::render::Ev;
use prelum_raster::{ColorSpace, Duplex};
use prelum_render::{parse_pages, pdf, render, sheet_count, RasterFormat, Scale, Settings};

/// A rewritten PDF is held whole until it is known to be complete (a page
/// may turn out to need raster); past this it is drawn as raster instead.
const MAX_PDF: usize = 256 << 20;
/// The document itself.
const MAX_INPUT: u64 = 2 << 30;

struct Out {
    w: io::Stdout,
    buf: Vec<u8>,
    bytes: u64,
}

impl Out {
    fn ev(&mut self, e: &Ev) -> io::Result<()> {
        frame::write(&mut self.w, e)
    }
    fn flush_data(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let d = std::mem::take(&mut self.buf);
            self.bytes += d.len() as u64;
            frame::write(&mut self.w, &Ev::Data(d))?;
        }
        Ok(())
    }
    fn data(&mut self, b: &[u8]) -> io::Result<()> {
        for piece in b.chunks(DATA_CHUNK) {
            self.buf.extend_from_slice(piece);
            if self.buf.len() >= DATA_CHUNK {
                self.flush_data()?;
            }
        }
        Ok(())
    }
}

/// The renderer's output: buffered into data frames.
struct Sink(Rc<RefCell<Out>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().data(b)?;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Args {
    format: String,
    fallback: String,
    dpi: u32,
    media: String,
    media_type: String,
    pages: String,
    per_sheet: u8,
    gray: bool,
    duplex: Duplex,
    sheet_back: String,
    scale: Scale,
    quality: u8,
    first: u32,
    count: u32,
    longest: u32,
}

fn keyword(s: &str) -> Result<String, String> {
    let ok = !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'));
    ok.then(|| s.to_string()).ok_or_else(|| format!("not a keyword: {s}"))
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        format: "pwg".into(),
        fallback: "pwg".into(),
        dpi: 300,
        media: "iso_a4_210x297mm".into(),
        media_type: "stationery".into(),
        pages: String::new(),
        per_sheet: 1,
        gray: false,
        duplex: Duplex::Simplex,
        sheet_back: "normal".into(),
        scale: Scale::ShrinkToFit,
        quality: 4,
        first: 1,
        count: 1,
        longest: 300,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().ok_or_else(|| format!("{k} needs a value"))?;
        match k.as_str() {
            "--format" => a.format = v,
            "--fallback" => a.fallback = v,
            "--dpi" => a.dpi = v.parse().ok().filter(|d| (72..=1200).contains(d)).ok_or("bad --dpi")?,
            "--media" => a.media = keyword(&v)?,
            "--media-type" => a.media_type = keyword(&v)?,
            "--pages" => a.pages = v,
            "--per-sheet" => a.per_sheet = v.parse().ok().filter(|n| [1, 2, 4].contains(n)).ok_or("bad --per-sheet")?,
            "--color" => a.gray = v == "gray",
            "--duplex" => {
                a.duplex = match v.as_str() {
                    "long" => Duplex::LongEdge,
                    "short" => Duplex::ShortEdge,
                    _ => Duplex::Simplex,
                }
            }
            "--sheet-back" => a.sheet_back = keyword(&v)?,
            "--scale" => {
                a.scale = match v.as_str() {
                    "fit" => Scale::Fit,
                    "shrink" => Scale::ShrinkToFit,
                    p => Scale::Percent(p.parse::<u16>().ok().filter(|p| (10..=400).contains(p)).ok_or("bad --scale")? as f64),
                }
            }
            "--first" => a.first = v.parse().ok().filter(|n| *n >= 1).ok_or("bad --first")?,
            "--count" => a.count = v.parse().ok().filter(|n| (1..=prelum_render::MAX_PREVIEWS).contains(n)).ok_or("bad --count")?,
            "--longest" => a.longest = v.parse().ok().filter(|n| (16..=prelum_render::MAX_PREVIEW).contains(n)).ok_or("bad --longest")?,
            "--quality" => a.quality = v.parse().ok().filter(|q| (3..=5).contains(q)).ok_or("bad --quality")?,
            _ => return Err(format!("unknown option {k}")),
        }
    }
    if !["pdf", "pwg", "urf", "preview"].contains(&a.format.as_str()) || !["pwg", "urf"].contains(&a.fallback.as_str()) {
        return Err("bad --format or --fallback".into());
    }
    Ok(a)
}

#[cfg(target_os = "freebsd")]
fn capsicum() -> io::Result<()> {
    extern "C" {
        fn cap_enter() -> libc::c_int;
    }
    // SAFETY: no arguments; only changes this process's mode.
    if unsafe { cap_enter() } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn capsicum() -> io::Result<()> {
    Ok(())
}

fn run(out: &Rc<RefCell<Out>>) -> Result<(), String> {
    let a = args()?;
    // SAFETY: fd 0 is ours, given by our parent; owned once, here.
    let file = unsafe { std::fs::File::from_raw_fd(0) };
    capsicum().map_err(|e| format!("cap_enter: {e}"))?;
    let len = file.metadata().map(|m| m.len()).map_err(|_| "the document is not a file".to_string())?;
    if len > MAX_INPUT {
        return Err("the document is too large to print".into());
    }
    let doc = folium_doc::Doc::open(Box::new(file), b"").map_err(|e| format!("the document can't be read: {e}"))?;
    let pages = parse_pages(&a.pages, doc.page_count()).map_err(|e| format!("pages: {e}"))?;
    let s = Settings {
        media: a.media,
        media_type: a.media_type,
        dpi: a.dpi,
        color: if a.gray { ColorSpace::SGray8 } else { ColorSpace::SRgb8 },
        scale: a.scale,
        pages,
        per_sheet: a.per_sheet,
        duplex: a.duplex,
        sheet_back: a.sheet_back,
        quality: a.quality,
        page_budget: Duration::from_secs(30),
    };
    let sheets = sheet_count(&s);
    let io = |e: io::Error| format!("prelumd went away: {e}");
    if a.format == "preview" {
        let (sheets, v) = prelum_render::preview(&doc, &s, a.first, a.count, a.longest, &|| false).map_err(|e| e.to_string())?;
        let mut o = out.borrow_mut();
        o.ev(&Ev::Sheets(sheets)).map_err(io)?;
        let n = v.len() as u32;
        for p in v {
            o.ev(&Ev::Preview { sheet: p.sheet, w: p.w, h: p.h, rgb: p.rgb }).map_err(io)?;
        }
        return o.ev(&Ev::Done { sheets: n, bytes: 0 }).map_err(io);
    }
    if a.format == "pdf" {
        match pdf::rewrite(&doc, &s, Vec::new(), &|| false) {
            Ok(bytes) if bytes.len() <= MAX_PDF => {
                let mut o = out.borrow_mut();
                o.ev(&Ev::Started { sheets, format: Format::Pdf }).map_err(io)?;
                o.data(&bytes).map_err(io)?;
                o.flush_data().map_err(io)?;
                for n in 1..=sheets {
                    o.ev(&Ev::Sheet(n)).map_err(io)?;
                }
                let bytes = o.bytes;
                return o.ev(&Ev::Done { sheets, bytes }).map_err(io);
            }
            Ok(_) => eprintln!("prelum-render: rewritten PDF too large, drawing pages instead"),
            Err(pdf::Error::NeedsRaster(why)) => eprintln!("prelum-render: {why}: drawing pages instead"),
            Err(e) => return Err(format!("the document can't be prepared: {e}")),
        }
    }
    let (rf, format) = match if a.format == "pdf" { a.fallback.as_str() } else { a.format.as_str() } {
        "urf" => (RasterFormat::Urf, Format::Urf),
        _ => (RasterFormat::Pwg, Format::PwgRaster),
    };
    out.borrow_mut().ev(&Ev::Started { sheets, format }).map_err(io)?;
    let progress_out = out.clone();
    let mut progress_err = None;
    let r = render(&doc, &s, rf, Sink(out.clone()), &|| false, &mut |n, _| {
        let mut o = progress_out.borrow_mut();
        if let Err(e) = o.flush_data().and_then(|_| o.ev(&Ev::Sheet(n))) {
            progress_err.get_or_insert(e);
        }
    });
    if let Some(e) = progress_err {
        return Err(io(e));
    }
    let (_, st) = r.map_err(|e| e.to_string())?;
    let mut o = out.borrow_mut();
    o.flush_data().map_err(io)?;
    let bytes = o.bytes;
    o.ev(&Ev::Done { sheets: st.sheets, bytes }).map_err(io)
}

fn main() {
    let out = Rc::new(RefCell::new(Out { w: io::stdout(), buf: Vec::new(), bytes: 0 }));
    if let Err(e) = run(&out) {
        eprintln!("prelum-render: {e}");
        let _ = out.borrow_mut().ev(&Ev::Failed(e));
        std::process::exit(1);
    }
}
