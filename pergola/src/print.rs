//! Printing from an app (printing.md §5.1): one call opens the system's
//! print sheet over the window, and [`Printing::handle`] follows it.
//!
//! The app hands over a PDF of what it wants printed (a file opened
//! read-only, or bytes made in memory: [`Printing::start_bytes`]); the
//! person chooses the printer and the options in the sheet, which the app
//! can't see into. The app hears only how it ended: queued, saved,
//! cancelled or failed. Save as PDF goes through the picker's Save sheet
//! here, so the app writes the file without learning where it is.
//!
//! The app shows its slot while printing (the same slot it gives the
//! picker: [`crate::view::Ctx::slot`] over the window) and passes every
//! event its [`Embeds`] poll returns to [`Printing::handle`] first.

use std::cell::RefCell;
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;

pub use limen::print::{Orientation, Paper};
use limen::print::{ToChild, ToParent, Why};
use limen::proto::{EmbedId, ParentEvent, WindowToken};

use crate::embed::{Embeds, Error};
use crate::slot::SlotKey;

/// How printing ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// With the printer's queue.
    Queued,
    /// Saved as a PDF where the person chose; its name.
    Saved(String),
    Cancelled,
    /// Plain words for the app's status line.
    Failed(String),
}

/// What [`Printing::handle`] made of an event of this print's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing for the app to do yet.
    Pending,
    /// Lay the document out again for this paper and answer with
    /// [`Printing::document`] (only when the app said it can).
    Relayout { paper: Paper, orientation: Orientation, margins_pt: u32 },
    Done(Outcome),
}

enum Stage {
    /// The sheet is open.
    Sheet,
    /// The sheet handed back the PDF and closed; the picker is choosing
    /// where it goes.
    Picking { file: std::fs::File, name: String, picker: EmbedId },
}

pub struct Printing {
    embed: EmbedId,
    window: WindowToken,
    slot: SlotKey,
    stage: Stage,
    /// Save as PDF's document, between the sheet's `SavePdf` and `Saved`.
    pending_save: Option<(std::fs::File, String)>,
}

/// A memory file holding `bytes`, sealed so nobody can change it: what
/// limend takes as a read-only document.
pub fn sealed_pdf(bytes: &[u8]) -> io::Result<OwnedFd> {
    limen::fdpass::sealed_file("print.pdf", bytes)
}

impl Printing {
    /// Open the print sheet in `slot` of the window `window` names for the
    /// PDF `pdf` (a regular file open read-only, or [`sealed_pdf`]).
    /// `can_relayout`: the app answers [`Step::Relayout`].
    pub fn start(embeds: &Rc<RefCell<Embeds>>, window: WindowToken, slot: SlotKey, pdf: BorrowedFd<'_>, title: &str, can_relayout: bool, hint: Option<Paper>) -> Result<Printing, Error> {
        let embed = embeds.borrow_mut().request("print", window, slot)?;
        let title: String = title.chars().filter(|c| !c.is_control()).take(limen::print::MAX_TITLE / 4).collect();
        let msg = ToChild::Print { title, page_size_hint: hint, can_relayout };
        if let Err(e) = embeds.borrow_mut().send_fd(embed, msg.encode(), pdf) {
            let _ = embeds.borrow_mut().detach_embed(embed);
            return Err(e);
        }
        Ok(Printing { embed, window, slot, stage: Stage::Sheet, pending_save: None })
    }

    /// [`Printing::start`] for a PDF made in memory.
    pub fn start_bytes(embeds: &Rc<RefCell<Embeds>>, window: WindowToken, slot: SlotKey, pdf: &[u8], title: &str, can_relayout: bool, hint: Option<Paper>) -> Result<Printing, Error> {
        let fd = sealed_pdf(pdf).map_err(Error::Io)?;
        Self::start(embeds, window, slot, fd.as_fd(), title, can_relayout, hint)
    }

    /// The embed the app's events for this print name.
    pub fn embed(&self) -> EmbedId {
        match &self.stage {
            Stage::Sheet => self.embed,
            Stage::Picking { picker, .. } => *picker,
        }
    }

    /// Whether `ev` is this print's; if so, what it means. Events of the
    /// window's other embeds come back `None`, for the app.
    pub fn handle(&mut self, embeds: &Rc<RefCell<Embeds>>, ev: &ParentEvent) -> Option<Step> {
        let mine = match ev {
            ParentEvent::Message { embed, .. } | ParentEvent::MessageFd { embed, .. } | ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed } => *embed == self.embed(),
            ParentEvent::Attached { embed } => return (*embed == self.embed()).then_some(Step::Pending),
            _ => false,
        };
        if !mine {
            return None;
        }
        Some(match self.stage {
            Stage::Sheet => self.from_sheet(embeds, ev),
            Stage::Picking { .. } => self.from_picker(embeds, ev),
        })
    }

    fn end(&mut self, embeds: &Rc<RefCell<Embeds>>, o: Outcome) -> Step {
        let _ = embeds.borrow_mut().detach_embed(self.embed());
        Step::Done(o)
    }

    fn from_sheet(&mut self, embeds: &Rc<RefCell<Embeds>>, ev: &ParentEvent) -> Step {
        let (bytes, fd) = match ev {
            ParentEvent::Message { bytes, .. } => (bytes, None),
            ParentEvent::MessageFd { bytes, .. } => (bytes, embeds.borrow_mut().take_fd()),
            _ => return self.end(embeds, Outcome::Failed("The print sheet closed".into())),
        };
        match ToParent::decode(bytes) {
            Some(ToParent::Relayout { paper, orientation, margins_pt }) => Step::Relayout { paper, orientation, margins_pt },
            Some(ToParent::SavePdf { name }) => {
                if let Some(fd) = fd {
                    self.pending_save = Some((std::fs::File::from(fd), name));
                }
                Step::Pending
            }
            Some(ToParent::Saved) => {
                let Some((file, name)) = self.pending_save.take() else {
                    return self.end(embeds, Outcome::Failed("The print sheet sent no PDF".into()));
                };
                let _ = embeds.borrow_mut().detach_embed(self.embed);
                // Where it goes: the picker's Save, in the same slot.
                let asked = embeds.borrow_mut().request("picker", self.window, self.slot);
                let asked = asked.and_then(|p| {
                    let f = limen::picker::Filter::save(&name, "");
                    embeds.borrow_mut().send(p, limen::picker::ToChild::Open(f).encode()).map(|_| p)
                });
                match asked {
                    Ok(picker) => {
                        self.stage = Stage::Picking { file, name, picker };
                        Step::Pending
                    }
                    Err(e) => Step::Done(Outcome::Failed(format!("The picker didn't open ({e:?})"))),
                }
            }
            Some(ToParent::Queued) => self.end(embeds, Outcome::Queued),
            Some(ToParent::Cancelled) => self.end(embeds, Outcome::Cancelled),
            Some(ToParent::Failed(why)) => self.end(embeds, Outcome::Failed(match why {
                Why::NotPdf => "This document couldn't be read for printing".into(),
                Why::Empty => "There is nothing to print".into(),
                Why::TooLarge => "This document is too large to print".into(),
            })),
            None => Step::Pending,
        }
    }

    fn from_picker(&mut self, embeds: &Rc<RefCell<Embeds>>, ev: &ParentEvent) -> Step {
        use limen::picker::ToParent as P;
        let (bytes, fd) = match ev {
            ParentEvent::Message { bytes, .. } => (bytes, None),
            ParentEvent::MessageFd { bytes, .. } => (bytes, embeds.borrow_mut().take_fd()),
            _ => return self.end(embeds, Outcome::Failed("The picker closed".into())),
        };
        match P::decode(bytes) {
            Some(P::File { name: saved, write: true, .. }) => {
                let Some(fd) = fd else { return self.end(embeds, Outcome::Failed("The picker sent no file".into())) };
                let Stage::Picking { file, .. } = &mut self.stage else { return Step::Pending };
                match copy(file, &mut std::fs::File::from(fd)) {
                    Ok(()) => self.end(embeds, Outcome::Saved(saved)),
                    Err(e) => self.end(embeds, Outcome::Failed(format!("Couldn't save “{saved}”: {e}"))),
                }
            }
            Some(P::Cancelled) => self.end(embeds, Outcome::Cancelled),
            Some(P::Failed { why }) => self.end(embeds, Outcome::Failed(why)),
            _ => Step::Pending,
        }
    }

    /// The answer to [`Step::Relayout`]: the document laid out again.
    pub fn document(&mut self, embeds: &Rc<RefCell<Embeds>>, pdf: BorrowedFd<'_>) -> Result<(), Error> {
        embeds.borrow_mut().send_fd(self.embed, ToChild::Document.encode(), pdf)
    }

    /// [`Printing::document`] for a PDF made in memory.
    pub fn document_bytes(&mut self, embeds: &Rc<RefCell<Embeds>>, pdf: &[u8]) -> Result<(), Error> {
        let fd = sealed_pdf(pdf).map_err(Error::Io)?;
        self.document(embeds, fd.as_fd())
    }

    /// Close the sheet (or the picker) without printing.
    pub fn cancel(self, embeds: &Rc<RefCell<Embeds>>) {
        let _ = embeds.borrow_mut().detach_embed(self.embed());
    }

    /// The name Save as PDF suggested, while the picker is open.
    pub fn saving(&self) -> Option<&str> {
        match &self.stage {
            Stage::Picking { name, .. } => Some(name),
            Stage::Sheet => None,
        }
    }
}

fn copy(from: &mut std::fs::File, to: &mut std::fs::File) -> io::Result<()> {
    from.rewind()?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n])?;
    }
    to.flush()
}

#[cfg(test)]
mod tests {
    //! Against the real limend code, with a fake frescod and a sheet and
    //! picker played by the test.
    use super::*;
    use limen::catalogue::{self, SlotKind};
    use limen::client::Child;
    use limen::proto::ChildEvent;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::time::Duration;

    const APP: u32 = 1001;
    const SHEET: u32 = 1800;
    const PICKER: u32 = 1700;
    const W: WindowToken = [1; 16];
    const W_SHEET: WindowToken = [8; 16];
    const W_PICK: WindowToken = [7; 16];
    const SLOT: SlotKey = 1;

    struct Fresco;
    impl limen::daemon::Compositor for Fresco {
        fn bind(&mut self, p: WindowToken, c: WindowToken, _k: SlotKind, _r: Option<[f32; 4]>, _z: u8) -> Result<(u32, u32), u8> {
            let owner = |w: WindowToken| match w {
                W => APP,
                W_SHEET => SHEET,
                _ => PICKER,
            };
            Ok((owner(p), owner(c)))
        }
        fn embed_connection(&mut self, _: u32) -> Result<UnixStream, u8> { Err(1) }
    }
    struct NoLaunch;
    impl limen::daemon::Launcher for NoLaunch {
        fn launch(&mut self, _: &str, _: EmbedId, _: UnixStream) -> Result<u32, String> { Err("none".into()) }
        fn end(&mut self, _: EmbedId) {}
    }

    fn conn(shared: &Arc<limen::daemon::Shared>, uid: u32) -> aqueduct::Connection {
        let (a, b) = UnixStream::pair().unwrap();
        let sh = shared.clone();
        std::thread::spawn(move || { let _ = limen::daemon::serve(aqueduct::Connection::wrap(b).unwrap(), uid, sh); });
        aqueduct::Connection::wrap(a).unwrap()
    }

    /// The child's next event, past the parent's persona (sent on attach).
    fn next(c: &mut Child) -> ChildEvent {
        loop {
            match c.poll(Duration::from_secs(2)).unwrap().expect("an event") {
                ChildEvent::ParentPersona { .. } => continue,
                e => return e,
            }
        }
    }

    /// Poll the app's embeds until printing says something other than Pending.
    fn run(p: &mut Printing, e: &Rc<RefCell<Embeds>>) -> Step {
        for _ in 0..200 {
            let ev = e.borrow_mut().poll(Duration::from_millis(20)).unwrap();
            if let Some(ev) = ev {
                match p.handle(e, &ev) {
                    Some(Step::Pending) | None => {}
                    Some(s) => return s,
                }
            }
        }
        panic!("printing said nothing");
    }

    #[test]
    fn print_relayout_and_save_as_pdf_through_the_picker() {
        let resolve = Box::new(|uid: u32| match uid {
            APP => Some(("ana".into(), "org.example.write".into())),
            SHEET => Some(("ana".into(), catalogue::PRELUM.into())),
            PICKER => Some(("ana".into(), catalogue::SCRINIUM.into())),
            _ => None,
        });
        let shared = limen::daemon::Shared::new(Box::new(Fresco), Box::new(NoLaunch), resolve, Box::new(|_: &str, _: &str| None));
        let mut sheet = Child::wrap(conn(&shared, SHEET));
        sheet.provide_role("print").unwrap();
        let mut picker = Child::wrap(conn(&shared, PICKER));
        picker.provide_role("picker").unwrap();
        let embeds = Embeds::wrap(limen::client::Parent::wrap(conn(&shared, APP)));
        // The slot is laid out (as the app's view would).
        crate::slot::SlotBinder::place(&mut *Embeds::binder(&embeds), 0, SLOT, Some([0.0, 0.0, 100.0, 100.0]));

        let mut p = Printing::start_bytes(&embeds, W, SLOT, b"%PDF-1.7 the app's", "Q4", true, Some(Paper::A4)).unwrap();
        let ChildEvent::Attach { embed, .. } = next(&mut sheet) else { panic!() };
        let ChildEvent::MessageFd { bytes, .. } = next(&mut sheet) else { panic!() };
        assert_eq!(ToChild::decode(&bytes), Some(ToChild::Print { title: "Q4".into(), page_size_hint: Some(Paper::A4), can_relayout: true }));
        let mut doc = String::new();
        std::fs::File::from(sheet.take_fd().unwrap()).read_to_string(&mut doc).unwrap();
        assert_eq!(doc, "%PDF-1.7 the app's");
        sheet.ready(embed, W_SHEET).unwrap();

        // Relayout, and the app's answer.
        let ask = ToParent::Relayout { paper: Paper::LETTER, orientation: Orientation::Landscape, margins_pt: 36 };
        sheet.emit(embed, ask.encode()).unwrap();
        assert_eq!(run(&mut p, &embeds), Step::Relayout { paper: Paper::LETTER, orientation: Orientation::Landscape, margins_pt: 36 });
        p.document_bytes(&embeds, b"%PDF-1.7 letter, landscape").unwrap();
        assert!(matches!(next(&mut sheet), ChildEvent::MessageFd { .. }));
        assert!(sheet.take_fd().is_some());

        // Save as PDF: the sheet hands it back and closes; the picker saves it.
        let path = std::env::temp_dir().join(format!("pergola-print-{}", std::process::id()));
        std::fs::write(&path, b"%PDF-1.7 as it would print").unwrap();
        let ro = std::fs::File::open(&path).unwrap();
        sheet.emit_fd(embed, ToParent::SavePdf { name: "Q4.pdf".into() }.encode(), ro.as_fd()).unwrap();
        sheet.emit(embed, ToParent::Saved.encode()).unwrap();
        let ChildEvent::Attach { embed: pe, role, .. } = (loop {
            let e = picker.poll(Duration::from_millis(50)).unwrap();
            if let Some(e) = e { break e; }
            let ev = embeds.borrow_mut().poll(Duration::from_millis(10)).unwrap();
            if let Some(ev) = ev { assert!(matches!(p.handle(&embeds, &ev), Some(Step::Pending) | None)); }
        }) else { panic!() };
        assert_eq!(role, "picker");
        assert_eq!(p.saving(), Some("Q4.pdf"));
        let ChildEvent::Message { bytes, .. } = next(&mut picker) else { panic!() };
        assert!(matches!(limen::picker::ToChild::decode(&bytes), Some(limen::picker::ToChild::Open(f)) if f.name == "Q4.pdf"));
        picker.ready(pe, W_PICK).unwrap();
        let out = std::env::temp_dir().join(format!("pergola-print-out-{}", std::process::id()));
        let rw = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&out).unwrap();
        // The slot binds once the picker's window is ready.
        let _ = embeds.borrow_mut().poll(Duration::from_millis(100));
        picker.emit_fd(pe, limen::picker::ToParent::File { name: "Q4 (2).pdf".into(), size: 0, write: true }.encode(), rw.as_fd()).unwrap();
        assert_eq!(run(&mut p, &embeds), Step::Done(Outcome::Saved("Q4 (2).pdf".into())));
        assert_eq!(std::fs::read(&out).unwrap(), b"%PDF-1.7 as it would print");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }
}
