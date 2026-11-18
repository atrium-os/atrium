//! The print sheet's state (printing.md §5.2), apart from drawing it: the
//! destinations (the person's saved printers, printers nearby, Save as
//! PDF), the options the chosen printer reports, the previews, and what a
//! press on Print does. `view` draws it; the binary runs it.

use prelum_ipp_codec::ops::{ColorMode, Sides};
use prelum_ipp_codec::printer::{PrinterState, Reason, Severity};
use prelum_proto::print::{Layout, Nearby, Print, Printer, Scale, SheetImage};

/// Where the document goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Dest {
    Saved(Printer),
    /// Found on the network: added to the person's printers when used.
    Nearby(Nearby),
    SavePdf,
}

/// Paper sizes the sheet offers, by PWG keyword, in this order.
pub const PAPERS: &[(&str, &str)] = &[
    ("iso_a4_210x297mm", "A4"),
    ("na_letter_8.5x11in", "Letter"),
    ("na_legal_8.5x14in", "Legal"),
    ("iso_a5_148x210mm", "A5"),
    ("iso_a3_297x420mm", "A3"),
];
/// The most paper sizes shown at once.
pub const MAX_PAPERS: usize = 4;
pub const MAX_COPIES: u32 = 99;

/// How a line of status is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Quiet,
    Warning,
    Error,
}

/// What a press on Print (or Save) does, once frescod has counted it.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Print(Print),
    /// A nearby printer: add it to the person's printers, then print there.
    AddThenPrint { uri: String, name: String, print: Print },
    /// Save as PDF: the rewritten document goes back to the app for the
    /// picker.
    Save { layout: Layout, name: String },
}

#[derive(Clone, Debug)]
pub struct Sheet {
    pub title: String,
    pub can_relayout: bool,
    pub dests: Vec<Dest>,
    pub chosen: usize,
    /// The saved printers came (until then: "Looking for printers…").
    pub listed: bool,
    pub copies: u32,
    pub all_pages: bool,
    pub range: String,
    pub gray: bool,
    pub sides: Sides,
    pub media: String,
    /// The paper came from the app, not from the sheet's default.
    pub media_hinted: bool,
    pub landscape: bool,
    pub per_sheet: u8,
    pub fit: bool,
    /// From the last preview: sheets in all, and which one is shown (from 1).
    pub sheets: u32,
    pub showing: u32,
    pub images: Vec<SheetImage>,
    pub preview_note: Option<String>,
    /// A line over the buttons: why a press didn't count, a refusal.
    pub notice: Option<(String, Tone)>,
    /// Something is under way ("Printing…"): the buttons wait.
    pub working: Option<&'static str>,
}

impl Sheet {
    pub fn new(title: &str, can_relayout: bool, hint: Option<limen::print::Paper>) -> Sheet {
        // The app's own paper if the sheet knows it, else A4; a hint wider
        // than high is that paper turned (a deck's slides).
        let near = |a: u32, b: u32, w: u32, h: u32| a.abs_diff(w) <= 2 && b.abs_diff(h) <= 2;
        let find = |turned: bool| hint.and_then(|p| PAPERS.iter().find(|(k, _)| paper_pt(k).is_some_and(|(w, h)| if turned { near(p.h_pt, p.w_pt, w, h) } else { near(p.w_pt, p.h_pt, w, h) })));
        let (media, landscape) = match find(false) {
            Some((k, _)) => (Some(k.to_string()), false),
            None => (find(true).map(|(k, _)| k.to_string()), hint.is_some_and(|p| p.w_pt > p.h_pt)),
        };
        let landscape = landscape && media.is_some();
        Sheet {
            title: title.into(),
            can_relayout,
            dests: vec![Dest::SavePdf],
            chosen: 0,
            listed: false,
            copies: 1,
            all_pages: true,
            range: String::new(),
            gray: false,
            sides: Sides::OneSided,
            media_hinted: media.is_some(),
            media: media.unwrap_or_else(|| PAPERS[0].0.into()),
            landscape,
            per_sheet: 1,
            fit: false,
            sheets: 0,
            showing: 1,
            images: Vec::new(),
            preview_note: None,
            notice: None,
            working: None,
        }
    }

    pub fn dest(&self) -> &Dest {
        &self.dests[self.chosen.min(self.dests.len() - 1)]
    }

    fn same(a: &Dest, b: &Dest) -> bool {
        match (a, b) {
            (Dest::Saved(x), Dest::Saved(y)) => x.id == y.id,
            (Dest::Nearby(x), Dest::Nearby(y)) => x.uri == y.uri,
            (Dest::SavePdf, Dest::SavePdf) => true,
            _ => false,
        }
    }

    /// The list again: saved printers, then nearby ones not saved, then
    /// Save as PDF. The chosen one stays chosen if it is still there.
    fn relist(&mut self, saved: Vec<Printer>, nearby: Vec<Nearby>) {
        let was = self.dest().clone();
        let first = !self.listed;
        let mut d: Vec<Dest> = saved.into_iter().map(Dest::Saved).collect();
        d.extend(nearby.into_iter().filter(|n| n.saved.is_none()).map(Dest::Nearby));
        d.push(Dest::SavePdf);
        self.dests = d;
        self.chosen = self.dests.iter().position(|x| Self::same(x, &was)).unwrap_or(0);
        // The first list: the first saved printer that can print now.
        if first {
            self.chosen = self.dests.iter().position(|x| matches!(x, Dest::Saved(p) if usable(p).is_ok())).unwrap_or(0);
            // No paper from the app: the printer's own.
            if let (false, Dest::Saved(p)) = (self.media_hinted, self.dest()) {
                if let Some(m) = p.media_default.clone().filter(|m| PAPERS.iter().any(|(k, _)| k == m)) {
                    self.media = m;
                }
            }
        }
        self.fit_options();
    }

    pub fn set_printers(&mut self, saved: Vec<Printer>) {
        let nearby = self.dests.iter().filter_map(|d| if let Dest::Nearby(n) = d { Some(n.clone()) } else { None }).collect();
        self.relist(saved, nearby);
        self.listed = true;
    }

    pub fn set_nearby(&mut self, nearby: Vec<Nearby>) {
        let saved = self.dests.iter().filter_map(|d| if let Dest::Saved(p) = d { Some(p.clone()) } else { None }).collect();
        self.relist(saved, nearby);
    }

    pub fn choose(&mut self, i: usize) {
        if i < self.dests.len() {
            self.chosen = i;
            self.notice = None;
            self.fit_options();
        }
    }

    /// Options the chosen destination doesn't offer go back to what it does.
    fn fit_options(&mut self) {
        let papers = self.papers();
        if !papers.iter().any(|(k, _)| *k == self.media) {
            let default = match self.dest() {
                Dest::Saved(p) => p.media_default.clone().filter(|m| papers.iter().any(|(k, _)| k == m)),
                _ => None,
            };
            self.media = default.unwrap_or_else(|| papers[0].0.to_string());
        }
        if !self.offers_color() {
            self.gray = true;
        }
        if !self.offers_duplex() {
            self.sides = Sides::OneSided;
        }
    }

    pub fn offers_color(&self) -> bool {
        match self.dest() {
            Dest::Saved(p) => p.color,
            Dest::Nearby(n) => n.color,
            Dest::SavePdf => true,
        }
    }

    pub fn offers_duplex(&self) -> bool {
        match self.dest() {
            Dest::Saved(p) => p.duplex,
            Dest::Nearby(n) => n.duplex,
            Dest::SavePdf => false,
        }
    }

    pub fn offers_copies(&self) -> bool {
        !matches!(self.dest(), Dest::SavePdf)
    }

    /// The paper sizes to offer: the ones the printer reports (or A4 and
    /// Letter when it reports none), at most [`MAX_PAPERS`].
    pub fn papers(&self) -> Vec<(&'static str, &'static str)> {
        let v: Vec<_> = match self.dest() {
            Dest::Saved(p) if !p.media_supported.is_empty() => PAPERS.iter().copied().filter(|(k, _)| p.media_supported.iter().any(|m| m == k)).collect(),
            Dest::SavePdf => PAPERS.to_vec(),
            _ => Vec::new(),
        };
        let mut v: Vec<_> = v.into_iter().take(MAX_PAPERS).collect();
        if v.is_empty() {
            v = PAPERS[..2].to_vec();
        }
        v
    }

    /// The page range, checked: `None` for all pages.
    pub fn pages(&self) -> Result<Option<String>, String> {
        if self.all_pages {
            return Ok(None);
        }
        let r: String = self.range.chars().filter(|c| !c.is_whitespace()).collect();
        let ok = !r.is_empty()
            && r.len() <= 200
            && r.split(',').all(|part| {
                let mut it = part.splitn(2, '-');
                let a = it.next().and_then(|a| a.parse::<u32>().ok());
                match (a, it.next()) {
                    (Some(a), None) => a >= 1,
                    (Some(a), Some(b)) => b.parse::<u32>().is_ok_and(|b| a >= 1 && b >= a),
                    _ => false,
                }
            });
        if ok {
            Ok(Some(r))
        } else {
            Err("Pages: numbers and ranges, such as 1-3, 5".into())
        }
    }

    /// How pages go on paper now: what the preview shows.
    pub fn layout(&self) -> Layout {
        Layout {
            media: self.media.clone(),
            // A bad range previews all pages; Print says what is wrong.
            pages: self.pages().ok().flatten(),
            per_sheet: self.per_sheet,
            scale: if self.fit { Scale::Fit } else { Scale::ShrinkToFit },
        }
    }

    /// What an app that can lay itself out again is asked for: the paper,
    /// turned for landscape.
    pub fn relayout(&self) -> Option<limen::print::ToParent> {
        if !self.can_relayout {
            return None;
        }
        let (w, h) = paper_pt(&self.media)?;
        Some(limen::print::ToParent::Relayout {
            paper: limen::print::Paper { w_pt: w, h_pt: h },
            orientation: if self.landscape { limen::print::Orientation::Landscape } else { limen::print::Orientation::Portrait },
            margins_pt: 36,
        })
    }

    /// The line under the printer list.
    pub fn status(&self) -> (String, Tone) {
        match self.dest() {
            Dest::SavePdf => ("Saves a PDF where you choose".into(), Tone::Quiet),
            Dest::Nearby(n) => {
                let mut s = "On this network: added to your printers when you print".to_string();
                if !n.tls {
                    s = format!("{s}. Sent unencrypted on this network");
                }
                (s, if n.tls { Tone::Quiet } else { Tone::Warning })
            }
            Dest::Saved(p) => {
                if let Err(why) = usable(p) {
                    return (why, Tone::Error);
                }
                let mut parts = Vec::new();
                let worst = p.reasons.iter().filter(|r| r.severity != Severity::Report).map(|r| words(r.reason)).next();
                parts.push(match (p.state, worst) {
                    (_, Some(w)) => w.to_string(),
                    (None, _) => "Offline".to_string(),
                    (Some(PrinterState::Stopped), _) => "Stopped".to_string(),
                    (Some(PrinterState::Processing), _) => "Printing something else".to_string(),
                    _ => "Ready".to_string(),
                });
                if p.new {
                    parts.push("not used before".into());
                }
                let mut tone = if worst.is_some() || p.state.is_none() { Tone::Warning } else { Tone::Quiet };
                if !p.tls {
                    parts.push("Sent unencrypted on this network".into());
                    tone = Tone::Warning;
                }
                (parts.join(". "), tone)
            }
        }
    }

    /// The words on the main button.
    pub fn button(&self) -> &'static str {
        match self.dest() {
            Dest::SavePdf => "Save…",
            _ => "Print",
        }
    }

    /// What a counted press does, or why it can't.
    pub fn action(&self) -> Result<Action, String> {
        let pages = self.pages()?;
        let print = |printer: u32| Print {
            printer,
            title: self.title.clone(),
            copies: self.copies.clamp(1, MAX_COPIES),
            sides: if self.offers_duplex() { self.sides } else { Sides::OneSided },
            color: if self.gray { ColorMode::Monochrome } else { ColorMode::Color },
            media: Some(self.media.clone()),
            quality: None,
            pages: pages.clone(),
            per_sheet: self.per_sheet,
            scale: if self.fit { Scale::Fit } else { Scale::ShrinkToFit },
        };
        match self.dest() {
            Dest::Saved(p) => usable(p).map(|_| Action::Print(print(p.id))),
            Dest::Nearby(n) => Ok(Action::AddThenPrint { uri: n.uri.clone(), name: n.name.clone(), print: print(0) }),
            Dest::SavePdf => Ok(Action::Save { layout: self.layout(), name: pdf_name(&self.title) }),
        }
    }

    /// The preview's sheet number moves within the document.
    pub fn step(&mut self, by: i32) {
        let n = (self.showing as i64 + by as i64).clamp(1, self.sheets.max(1) as i64);
        self.showing = n as u32;
    }

    /// The preview to show, if it came.
    pub fn image(&self) -> Option<&SheetImage> {
        self.images.iter().find(|i| i.sheet == self.showing)
    }
}

/// Whether a saved printer can take a job now, or why not.
pub fn usable(p: &Printer) -> Result<(), String> {
    if p.changed.is_some() {
        return Err("This printer's identity changed: check it in Settings, Printers".into());
    }
    if p.elsewhere {
        return Err("Not on this network".into());
    }
    Ok(())
}

fn words(r: Reason) -> &'static str {
    match r {
        Reason::MediaEmpty | Reason::MediaNeeded => "Out of paper",
        Reason::MediaLow => "Paper low",
        Reason::MediaJam => "Paper jam",
        Reason::TonerLow | Reason::MarkerSupplyLow => "Ink or toner low",
        Reason::TonerEmpty | Reason::MarkerSupplyEmpty => "Out of ink or toner",
        Reason::DoorOpen | Reason::CoverOpen => "A door or cover is open",
        Reason::InputTrayMissing => "A paper tray is missing",
        Reason::OutputAreaFull => "The output tray is full",
        Reason::Offline | Reason::Shutdown => "Offline",
        Reason::Paused => "Paused",
        Reason::Other => "Needs attention",
    }
}

/// A PWG media keyword's size in points, portrait.
pub fn paper_pt(k: &str) -> Option<(u32, u32)> {
    let d = k.rsplit('_').next()?;
    let (d, per) = if let Some(d) = d.strip_suffix("mm") { (d, 72.0 / 25.4) } else { (d.strip_suffix("in")?, 72.0) };
    let (a, b) = d.split_once('x')?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    let (w, h) = ((a * per).round() as u32, (b * per).round() as u32);
    (w > 0 && h > 0).then_some((w.min(h), w.max(h)))
}

/// "Q4 report" → "Q4 report.pdf", with nothing a file name can't hold.
pub fn pdf_name(title: &str) -> String {
    let t: String = title.chars().filter(|c| !c.is_control() && *c != '/').take(120).collect();
    let t = t.trim().trim_start_matches('.');
    let t = t.strip_suffix(".pdf").unwrap_or(t);
    if t.is_empty() {
        "Document.pdf".into()
    } else {
        format!("{t}.pdf")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_turned_paper_hint_starts_landscape() {
        let s = Sheet::new("Deck", true, Some(limen::print::Paper { w_pt: 842, h_pt: 595 }));
        assert!(s.landscape);
        assert!(s.media_hinted);
        let s = Sheet::new("Doc", true, Some(limen::print::Paper::A4));
        assert!(!s.landscape);
    }

    use super::*;
    use prelum_ipp_codec::printer::StateReason;

    fn printer(id: u32, name: &str) -> Printer {
        Printer {
            id,
            name: name.into(),
            uri: format!("ipps://10.0.0.{id}:631/ipp/print"),
            tls: true,
            fingerprint: None,
            uuid: None,
            make_and_model: String::new(),
            location: String::new(),
            network: "home".into(),
            elsewhere: false,
            state: Some(PrinterState::Idle),
            reasons: Vec::new(),
            markers: Vec::new(),
            media_supported: vec!["iso_a4_210x297mm".into(), "na_letter_8.5x11in".into(), "iso_a5_148x210mm".into()],
            media_default: Some("na_letter_8.5x11in".into()),
            duplex: true,
            color: true,
            new: false,
            changed: None,
        }
    }

    fn nearby(uri: &str, saved: Option<u32>) -> Nearby {
        Nearby {
            uri: uri.into(),
            name: "Office".into(),
            make_and_model: String::new(),
            location: String::new(),
            announced: "Office".into(),
            tls: false,
            fingerprint: None,
            uuid: None,
            color: false,
            duplex: false,
            saved,
        }
    }

    #[test]
    fn the_list_is_saved_then_nearby_then_save_as_pdf() {
        let mut s = Sheet::new("Q4", false, None);
        assert_eq!(s.dests, vec![Dest::SavePdf], "until the list comes");
        let mut broken = printer(1, "Moved");
        broken.elsewhere = true;
        s.set_printers(vec![broken, printer(2, "Home")]);
        assert_eq!(s.chosen, 1, "the first printer that can print now");
        assert_eq!(s.media, "na_letter_8.5x11in", "its default paper");
        s.set_nearby(vec![nearby("ipp://10.0.0.9:631/ipp/print", None), nearby("ipps://10.0.0.2:631/ipp/print", Some(2))]);
        assert_eq!(s.dests.len(), 4, "a nearby printer already saved is listed once");
        assert!(matches!(s.dest(), Dest::Saved(p) if p.name == "Home"), "the choice stays");
        assert!(matches!(s.dests[2], Dest::Nearby(_)) && matches!(s.dests[3], Dest::SavePdf));
    }

    #[test]
    fn options_follow_the_printer() {
        let mut s = Sheet::new("Q4", false, Some(limen::print::Paper::A4));
        assert_eq!(s.media, "iso_a4_210x297mm", "the app's paper");
        s.set_printers(vec![printer(1, "Home")]);
        assert_eq!(s.media, "iso_a4_210x297mm", "kept: the printer has it");
        assert_eq!(s.papers().iter().map(|p| p.1).collect::<Vec<_>>(), vec!["A4", "Letter", "A5"]);
        s.sides = Sides::TwoSidedLongEdge;
        s.set_nearby(vec![nearby("ipp://10.0.0.9:631/ipp/print", None)]);
        s.choose(1);
        assert!(s.gray && s.sides == Sides::OneSided, "no colour, no two-sided there");
        let (line, tone) = s.status();
        assert!(line.contains("unencrypted") && tone == Tone::Warning, "{line}");
        s.choose(2);
        assert_eq!(s.button(), "Save…");
        assert!(!s.offers_copies());
    }

    #[test]
    fn a_press_prints_adds_or_saves() {
        let mut s = Sheet::new("Q4 report", false, None);
        s.set_printers(vec![printer(7, "Home")]);
        s.copies = 3;
        s.all_pages = false;
        s.range = "1-2, 4".into();
        match s.action().unwrap() {
            Action::Print(p) => {
                assert_eq!((p.printer, p.copies, p.pages.as_deref(), p.title.as_str()), (7, 3, Some("1-2,4"), "Q4 report"));
            }
            a => panic!("{a:?}"),
        }
        s.range = "4-2".into();
        assert!(s.action().is_err(), "a backwards range");
        s.range = "x".into();
        assert!(s.action().is_err());
        assert_eq!(s.layout().pages, None, "the preview shows every page meanwhile");
        s.all_pages = true;
        s.set_nearby(vec![nearby("ipp://10.0.0.9:631/ipp/print", None)]);
        s.choose(1);
        assert!(matches!(s.action().unwrap(), Action::AddThenPrint { ref uri, .. } if uri == "ipp://10.0.0.9:631/ipp/print"));
        s.choose(2);
        assert_eq!(s.action().unwrap(), Action::Save { layout: s.layout(), name: "Q4 report.pdf".into() });
        // A printer whose identity changed takes nothing.
        let mut p = printer(8, "Changed");
        p.changed = Some(prelum_proto::print::Changed { fingerprint: None, uuid: None });
        s.set_printers(vec![p]);
        s.choose(0);
        assert!(s.action().is_err());
        assert_eq!(s.status().1, Tone::Error);
    }

    #[test]
    fn status_words() {
        let mut s = Sheet::new("t", false, None);
        let mut p = printer(1, "Home");
        p.reasons = vec![StateReason { reason: Reason::MediaEmpty, severity: Severity::Error }];
        p.new = true;
        s.set_printers(vec![p]);
        assert_eq!(s.status(), ("Out of paper. not used before".into(), Tone::Warning));
    }

    #[test]
    fn relayout_paper_and_names() {
        assert_eq!(paper_pt("iso_a4_210x297mm"), Some((595, 842)));
        assert_eq!(paper_pt("na_letter_8.5x11in"), Some((612, 792)));
        assert_eq!(paper_pt("letter"), None);
        let mut s = Sheet::new("t", true, None);
        s.landscape = true;
        assert_eq!(
            s.relayout(),
            Some(limen::print::ToParent::Relayout { paper: limen::print::Paper::A4, orientation: limen::print::Orientation::Landscape, margins_pt: 36 })
        );
        assert_eq!(Sheet::new("t", false, None).relayout(), None);
        assert_eq!(pdf_name("Q4.pdf"), "Q4.pdf");
        assert_eq!(pdf_name("../a/b"), "ab.pdf");
        assert_eq!(pdf_name(""), "Document.pdf");
    }
}
