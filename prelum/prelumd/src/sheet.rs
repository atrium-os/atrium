//! What prelum-sheet asks of prelumd besides printing (printing.md §5.2):
//! previews drawn in a render jail, the rewritten PDF for Save as PDF, and
//! acting for the person whose app opened the sheet.
//!
//! The sheet never reads the document: prelumd hands the app's descriptor
//! to a `prelum-render` jail and passes back what comes out, small images
//! or the rewritten PDF, without looking at either beyond their sizes.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

use prelum_proto::frame;
use prelum_proto::print::{self, op, status, Layout, PreviewReq, Previews, SheetImage};
use prelum_proto::render;

use crate::daemon::{media_ok, short, Daemon, Fail};

/// A preview or a rewrite that takes longer than this is given up.
const RENDER_FOR: Duration = Duration::from_secs(60);
/// The largest rewritten PDF Save as PDF hands back (prelum-render's own cap).
const MAX_SAVED: u64 = 256 << 20;

fn bad<T>(m: &str) -> Result<T, Fail> {
    Err((status::MALFORMED, m.into()))
}

/// The render worker's arguments for `l`, after checking it.
fn layout_args(l: &Layout) -> Result<Vec<String>, Fail> {
    if !media_ok(&l.media) {
        return bad("bad paper size");
    }
    if l.pages.as_ref().is_some_and(|p| p.is_empty() || p.len() > 256 || !p.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b',' || b == b' ')) {
        return bad("bad page range");
    }
    if ![1, 2, 4].contains(&l.per_sheet) {
        return bad("bad pages per sheet");
    }
    let mut a = vec!["--media".to_string(), l.media.clone(), "--per-sheet".into(), l.per_sheet.to_string(), "--scale".into()];
    a.push(match l.scale {
        print::Scale::Fit => "fit".into(),
        print::Scale::ShrinkToFit => "shrink".into(),
        print::Scale::Percent(p) => p.clamp(10, 400).to_string(),
    });
    if let Some(p) = &l.pages {
        a.extend(["--pages".into(), p.replace(' ', "")]);
    }
    Ok(a)
}

fn document(pdf: OwnedFd) -> Result<OwnedFd, Fail> {
    let f = File::from(pdf);
    match f.metadata() {
        Ok(m) if m.is_file() && m.len() > 0 => Ok(f.into()),
        Ok(m) if m.is_file() => Err((status::MALFORMED, "the document is empty".into())),
        _ => bad("the document must be a file"),
    }
}

impl Daemon {
    /// The person (and persona) a sheet acts for: `app_uid`'s, if the peer
    /// is prelum-sheet and the launch registry knows the app.
    pub fn act_for(&self, peer: u32, app_uid: u32) -> Result<(u32, String), Fail> {
        if self.cfg().sheet_uid != Some(peer) {
            return Err((status::NOT_ALLOWED, "only the print sheet acts for an app".into()));
        }
        match (self.cfg().person_of)(app_uid) {
            Some((person, persona, _)) => Ok((person, persona.unwrap_or_default())),
            None => Err((status::NOT_ALLOWED, "not an app".into())),
        }
    }

    /// Who a peer that is not the sheet is: the person themselves, Curia or
    /// prelum-status acting for theirs, or another app (refused).
    pub fn caller(&self, peer: u32) -> Caller {
        match (self.cfg().person_of)(peer) {
            None => Caller::Person,
            Some((person, persona, app)) if app == crate::daemon::CURIA_APP => Caller::Curia { person, persona: persona.unwrap_or_default() },
            Some((person, _, app)) if app == crate::daemon::STATUS_APP => Caller::Status { person },
            Some(_) => Caller::App,
        }
    }


    /// Small images of the sheets asked for, drawn in a render jail.
    pub fn preview(&self, r: &PreviewReq, pdf: OwnedFd) -> Result<Previews, Fail> {
        let mut args = vec!["--format".to_string(), "preview".into()];
        args.extend(layout_args(&r.layout)?);
        if r.first == 0 || r.count == 0 || r.count > print::MAX_PREVIEWS || !(16..=print::MAX_PREVIEW).contains(&r.longest) {
            return bad("bad preview request");
        }
        args.extend(["--first".into(), r.first.to_string(), "--count".into(), r.count.to_string(), "--longest".into(), r.longest.to_string()]);
        let p = self.launcher().render(document(pdf)?, &args).map_err(|e| (status::FAILED, format!("the preview could not start: {e}")))?;
        let mut sock = p.sock;
        let _ = sock.set_read_timeout(Some(RENDER_FOR));
        let mut out = Previews { sheets: 0, images: Vec::new() };
        let r = loop {
            match frame::read::<render::Ev>(&mut sock) {
                Ok(Some(render::Ev::Sheets(n))) => out.sheets = n,
                Ok(Some(render::Ev::Preview { sheet, w, h, rgb })) => {
                    // What the jail says is only passed on; its shape is checked.
                    let ok = w > 0 && h > 0 && w.max(h) <= print::MAX_PREVIEW as u32 && rgb.len() as u64 == w as u64 * h as u64 * 3;
                    if !ok || out.images.len() >= r.count as usize {
                        break Err((status::FAILED, "the preview came out wrong".to_string()));
                    }
                    out.images.push(SheetImage { sheet, w, h, rgb });
                }
                Ok(Some(render::Ev::Done { .. })) => break Ok(()),
                Ok(Some(render::Ev::Failed(e))) => break Err((status::FAILED, short(&e))),
                Ok(Some(_)) => break Err((status::FAILED, "the renderer answered out of turn".into())),
                Ok(None) | Err(_) => break Err((status::FAILED, "the document could not be read".into())),
            }
        };
        p.stop.stop();
        r.map(|_| out)
    }

    /// The document rewritten with `l` (§6.2), in an unnamed file in the
    /// spool, for Save as PDF.
    pub fn save_pdf(&self, l: &Layout, pdf: OwnedFd) -> Result<(u64, File), Fail> {
        let mut args = vec!["--format".to_string(), "pdf".into(), "--fallback".into(), "pwg".into()];
        args.extend(layout_args(l)?);
        let mut out = unnamed(&self.cfg().spool).map_err(|e| (status::FAILED, format!("spool: {e}")))?;
        let p = self.launcher().render(document(pdf)?, &args).map_err(|e| (status::FAILED, format!("the document could not be prepared: {e}")))?;
        let mut sock = p.sock;
        let _ = sock.set_read_timeout(Some(RENDER_FOR));
        let mut n = 0u64;
        let r = loop {
            match frame::read::<render::Ev>(&mut sock) {
                Ok(Some(render::Ev::Started { format, .. })) if format != prelum_ipp_codec::ops::Format::Pdf => {
                    break Err((status::FAILED, "This document can't be saved as a PDF with these settings: try all pages, one per sheet.".to_string()));
                }
                Ok(Some(render::Ev::Started { .. })) | Ok(Some(render::Ev::Sheet(_))) => {}
                Ok(Some(render::Ev::Data(d))) => {
                    n += d.len() as u64;
                    if n > MAX_SAVED {
                        break Err((status::FAILED, "the PDF is too large".into()));
                    }
                    if let Err(e) = out.write_all(&d) {
                        break Err((status::FAILED, format!("spool: {e}")));
                    }
                }
                Ok(Some(render::Ev::Done { .. })) => break Ok(()),
                Ok(Some(render::Ev::Failed(e))) => break Err((status::FAILED, short(&e))),
                Ok(Some(_)) => break Err((status::FAILED, "the renderer answered out of turn".into())),
                Ok(None) | Err(_) => break Err((status::FAILED, "the document could not be read".into())),
            }
        };
        p.stop.stop();
        r?;
        out.seek(SeekFrom::Start(0)).map_err(|e| (status::FAILED, format!("spool: {e}")))?;
        Ok((n, out))
    }
}

/// A connection's caller, other than the sheet (§6.4's clients).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// Not an app: the person (prelumctl, the console).
    Person,
    /// Curia's Printers page: everything but printing.
    Curia { person: u32, persona: String },
    /// prelum-status: jobs, cancel, release, printers' names.
    Status { person: u32 },
    /// Any other app: apps print through the sheet.
    App,
}

impl Caller {
    pub fn may(&self, o: u16) -> bool {
        match self {
            Caller::Person => true,
            Caller::Curia { .. } => !matches!(o, op::PRINT | op::PREVIEW | op::SAVE_PDF),
            Caller::Status { .. } => matches!(o, op::PRINTERS | op::JOBS | op::WATCH | op::CANCEL | op::RELEASE),
            Caller::App => false,
        }
    }
}

/// A new file in `dir` with no name: created, opened for reading and
/// writing, and unlinked at once, so it goes when the last descriptor does.
fn unnamed(dir: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    for _ in 0..16 {
        let mut b = [0u8; 8];
        getrandom(&mut b);
        let p = dir.join(format!(".save-{:016x}", u64::from_le_bytes(b)));
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&p) {
            Ok(f) => {
                std::fs::remove_file(&p)?;
                return Ok(f);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name"))
}

fn getrandom(b: &mut [u8]) {
    use std::io::Read;
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        b.copy_from_slice(&(t ^ std::process::id() as u64).to_le_bytes()[..b.len().min(8)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_checked() {
        let mut l = Layout::new("iso_a4_210x297mm");
        assert!(layout_args(&l).is_ok());
        l.pages = Some("1-3, 5".into());
        let a = layout_args(&l).unwrap();
        assert!(a.windows(2).any(|w| w == ["--pages", "1-3,5"]));
        l.pages = Some("1;rm".into());
        assert!(layout_args(&l).is_err());
        l.pages = None;
        l.per_sheet = 3;
        assert!(layout_args(&l).is_err());
        l.per_sheet = 2;
        l.media = "a4".into();
        assert!(layout_args(&l).is_err());
    }

    #[test]
    fn an_unnamed_file_has_no_name() {
        let d = std::env::temp_dir().join(format!("prelum-unnamed-{}", std::process::id()));
        let mut f = unnamed(&d).unwrap();
        f.write_all(b"%PDF").unwrap();
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        let _ = std::fs::remove_dir(&d);
    }
}
