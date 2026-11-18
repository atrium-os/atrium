//! The Printers page (printing.md §6.4, §6.5, §9): the person's saved
//! printers (identify, rename, share with every persona, remove), adding
//! one nearby or by address, and print history with Clear. prelumd knows
//! Curia by the launch registry (`printers-configure`) and gives it every
//! printers op but printing. `main.rs` runs [`work`] for each [`Want`] and
//! hands back what it [`Got`]; the drawing is [`CuriaView::printers`].

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::theme::{type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor as ph, ScrollView};
use pergola::Mutable;
use prelum_proto::print::{client::Client, Add, Ended, Nearby, Phase, Printer, Rename, Share};

use crate::view::{button, em, icon, rect, text, width, CuriaView, Look};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Refresh,
    Nearby,
    Add(String),
    Identify(u32),
    Rename(u32, String),
    Share(u32, bool),
    Remove(u32),
    ClearHistory,
}

#[derive(Clone, Debug)]
pub enum Got {
    Printers(Result<Vec<Printer>, String>),
    History(Result<Vec<Ended>, String>),
    Nearby(Result<Vec<Nearby>, String>),
    /// A change: what to say, or why it failed.
    Done(Result<String, String>),
}

#[derive(Clone, Debug, Default)]
pub struct PrintersPage {
    /// None until prelumd answers.
    pub printers: Option<Result<Vec<Printer>, String>>,
    pub history: Option<Result<Vec<Ended>, String>>,
    pub nearby: Option<Result<Vec<Nearby>, String>>,
    pub looking: bool,
    /// A change on its way (nothing else is offered meanwhile).
    pub busy: bool,
    /// The last change's outcome: (failed, words).
    pub note: Option<(bool, String)>,
    /// The printer whose name is being changed.
    pub renaming: Option<u32>,
    /// Remove pressed once: press again to remove.
    pub confirm_remove: Option<u32>,
    pub confirm_clear: bool,
    wants: Vec<Want>,
}

impl PrintersPage {
    pub fn refresh(&mut self) {
        if !self.wants.contains(&Want::Refresh) {
            self.wants.push(Want::Refresh);
        }
    }

    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    fn change(&mut self, w: Want) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.note = None;
        self.wants.push(w);
    }

    pub fn look_nearby(&mut self) {
        if !self.looking {
            self.looking = true;
            self.wants.push(Want::Nearby);
        }
    }

    pub fn add(&mut self, uri: &str) {
        let uri = uri.trim();
        if !uri.is_empty() {
            self.change(Want::Add(uri.to_string()));
        }
    }

    pub fn identify(&mut self, id: u32) {
        self.change(Want::Identify(id));
    }

    pub fn rename(&mut self, id: u32, name: &str) {
        let name = name.trim();
        if !name.is_empty() {
            self.renaming = None;
            self.change(Want::Rename(id, name.to_string()));
        }
    }

    pub fn share(&mut self, id: u32, shared: bool) {
        self.change(Want::Share(id, shared));
    }

    /// The first press asks; the second removes.
    pub fn remove(&mut self, id: u32) {
        if self.confirm_remove == Some(id) {
            self.confirm_remove = None;
            self.change(Want::Remove(id));
        } else {
            self.confirm_remove = Some(id);
        }
    }

    pub fn clear_history(&mut self) {
        if self.confirm_clear {
            self.confirm_clear = false;
            self.change(Want::ClearHistory);
        } else {
            self.confirm_clear = true;
        }
    }

    pub fn apply(&mut self, g: Got) {
        match g {
            Got::Printers(r) => self.printers = Some(r),
            Got::History(r) => self.history = Some(r),
            Got::Nearby(r) => {
                self.looking = false;
                self.nearby = Some(r);
            }
            Got::Done(r) => {
                self.busy = false;
                self.note = Some(match r {
                    Ok(m) => (false, m),
                    Err(m) => (true, m),
                });
                self.refresh();
            }
        }
    }
}

fn sock() -> PathBuf {
    std::env::var_os("CURIA_PRINT_SOCK").map(PathBuf::from).unwrap_or_else(|| "/atrium/sockets/print/print.sock".into())
}

/// One ask of prelumd, on a thread of its own. What it says is logged as
/// `curia: printers: …` (the VM check reads it).
pub fn work(w: Want, names: Vec<(u32, String)>, tx: Sender<Got>) {
    std::thread::spawn(move || {
        let connect = || Client::connect(&sock()).map_err(|e| format!("prelumd is not running ({e})"));
        let name = |id: u32| names.iter().find(|(i, _)| *i == id).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("printer {id}"));
        let done = |what: String, r: Result<String, String>| {
            match &r {
                Ok(_) => eprintln!("curia: printers: {what}"),
                Err(e) => eprintln!("curia: printers: {what} failed: {e}"),
            }
            let _ = tx.send(Got::Done(r.map(|_| what)));
        };
        match w {
            Want::Refresh => {
                let (p, h) = match connect() {
                    Ok(mut c) => (c.printers().map_err(|e| e.to_string()), c.history().map_err(|e| e.to_string())),
                    Err(e) => (Err(e.clone()), Err(e)),
                };
                let _ = tx.send(Got::Printers(p));
                let _ = tx.send(Got::History(h));
            }
            Want::Nearby => {
                let r = connect().and_then(|mut c| c.nearby(4).map_err(|e| e.to_string()));
                if let Ok(v) = &r {
                    eprintln!("curia: printers: {} nearby", v.len());
                }
                let _ = tx.send(Got::Nearby(r));
            }
            Want::Add(uri) => {
                let r = connect().and_then(|mut c| c.add(&Add { uri: uri.clone(), name: String::new() }).map_err(|e| e.to_string()));
                match r {
                    Ok(p) => done(format!("added {} ({})", p.name, p.uri), Ok(String::new())),
                    Err(e) => done(format!("add {uri}"), Err(e)),
                }
            }
            Want::Identify(id) => done(format!("identified {}", name(id)), connect().and_then(|mut c| c.identify(id).map_err(|e| e.to_string()))),
            Want::Rename(id, to) => {
                let r = connect().and_then(|mut c| c.rename(&Rename { printer: id, name: to.clone() }).map_err(|e| e.to_string()));
                done(format!("renamed {} to {to}", name(id)), r)
            }
            Want::Share(id, shared) => {
                let r = connect().and_then(|mut c| c.share(&Share { printer: id, shared }).map_err(|e| e.to_string()));
                done(format!("{} {}", name(id), if shared { "is shared by every persona" } else { "is its persona's only" }), r)
            }
            Want::Remove(id) => done(format!("removed {}", name(id)), connect().and_then(|mut c| c.remove(id).map_err(|e| e.to_string()))),
            Want::ClearHistory => done("history cleared".into(), connect().and_then(|mut c| c.clear_history().map_err(|e| e.to_string()))),
        }
    });
}

pub fn with_printers(page: &Mutable<PrintersPage>, f: impl FnOnce(&mut PrintersPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

/// A printer's second line: whose, how it is, where.
pub fn describe(p: &Printer) -> String {
    let whose = if p.shared {
        "Shared".to_string()
    } else if p.persona.is_empty() {
        "Personal".to_string()
    } else {
        p.persona.clone()
    };
    let state = match (&p.changed, p.elsewhere, p.state) {
        (Some(_), _, _) => "not the printer you saved".to_string(),
        (_, true, _) => "not on this network".to_string(),
        (_, _, None) => "offline".to_string(),
        (_, _, Some(s)) => format!("{s:?}").to_lowercase(),
    };
    let tls = if p.tls { "" } else { " \u{b7} not encrypted" };
    format!("{whose} \u{b7} {state} \u{b7} {}{tls}", p.uri)
}

/// A history line's result, in words.
pub fn result(e: &Ended) -> String {
    let what = match e.result {
        Phase::Done => format!("Printed, {} {}", e.sheets, if e.sheets == 1 { "sheet" } else { "sheets" }),
        Phase::Cancelled => "Cancelled".to_string(),
        _ if e.message.is_empty() => "Failed".to_string(),
        _ => format!("Failed: {}", e.message),
    };
    let persona = if e.persona.is_empty() { "Personal" } else { &e.persona };
    format!("{what} \u{b7} {} \u{b7} {persona}", e.printer)
}

/// When, roughly, from seconds since the epoch.
pub fn ago(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// The row buttons' slots, left to right from the row's left edge.
pub const SLOT: f32 = 120.0;

impl CuriaView {
    /// The Printers page. Each saved printer is a card: its name, whose it
    /// is and how it is, and Identify, Rename, Share, Remove in fixed slots
    /// (Remove asks again in the same place).
    pub(crate) fn printers(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.printers.get_cloned();
        let t = ctx.theme;
        let y = em(20.0);
        text(ctx, x0, y + em(16.0), "Printers", type_size::XL, Weight::Semibold, t.text_primary());
        let top = y + em(48.0);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let page = self.printers.clone();
        let (rename_field, add_field) = (self.printer_fields[0].clone(), self.printer_fields[1].clone());
        let content = pergola::view::Draw(move |ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let mut y = 0.0;
            // The last change's outcome.
            match (&p.note, p.busy) {
                (_, true) => {
                    text(ctx, 0.0, y + em(12.0), "Working…", type_size::SM, Weight::Regular, t.text_tertiary());
                }
                (Some((failed, m)), _) => {
                    text(ctx, 0.0, y + em(12.0), m, type_size::SM, Weight::Regular, if *failed { t.danger_fg() } else { t.text_secondary() });
                }
                _ => {}
            }
            y += em(28.0);
            let list = match &p.printers {
                None => {
                    text(ctx, 0.0, y + em(10.0), "Asking for your printers…", type_size::SM, Weight::Regular, t.text_tertiary());
                    Vec::new()
                }
                Some(Err(why)) => {
                    text(ctx, 0.0, y + em(10.0), &format!("Printers are not available: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                    Vec::new()
                }
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "No printers yet. Add one below, or from any app's print sheet.", type_size::SM, Weight::Regular, t.text_secondary());
                    Vec::new()
                }
                Some(Ok(v)) => v.clone(),
            };
            if list.is_empty() {
                y += em(28.0);
            }
            for pr in &list {
                let renaming = p.renaming == Some(pr.id);
                let rh = em(if renaming { 136.0 } else { 96.0 });
                rect(ctx, 0.0, y, pw, rh, t.bg_surface(), 8.0);
                icon(ctx, em(16.0), y + em(14.0), ph::FILES, em(20.0), t.text_secondary());
                text(ctx, em(48.0), y + em(24.0), &pr.name, type_size::MD, Weight::Medium, t.text_primary());
                let warn = pr.changed.is_some() || pr.state.is_none();
                text(ctx, em(48.0), y + em(44.0), &describe(pr), type_size::SM, Weight::Regular, if warn { t.warning_fg() } else { t.text_secondary() });
                let by = y + em(56.0);
                let id = pr.id;
                let enabled = !p.busy;
                let pg = page.clone();
                button(ctx, em(16.0), by, "Identify", Look::Secondary, enabled, move || with_printers(&pg, |a| a.identify(id)));
                let (pg, f) = (page.clone(), rename_field.clone());
                button(ctx, em(16.0 + SLOT), by, "Rename", Look::Secondary, enabled, move || {
                    f.set(Default::default());
                    with_printers(&pg, |a| a.renaming = Some(id))
                });
                let (pg, shared) = (page.clone(), pr.shared);
                button(ctx, em(16.0 + 2.0 * SLOT), by, if shared { "Stop sharing" } else { "Share" }, Look::Secondary, enabled, move || with_printers(&pg, |a| a.share(id, !shared)));
                let pg = page.clone();
                let confirm = p.confirm_remove == Some(id);
                button(ctx, em(16.0 + 3.0 * SLOT), by, if confirm { "Remove" } else { "Remove…" }, if confirm { Look::Danger } else { Look::Secondary }, enabled, move || with_printers(&pg, |a| a.remove(id)));
                if confirm {
                    let w = "Its settings go; its waiting jobs are cancelled.";
                    text(ctx, em(16.0 + 4.0 * SLOT), by + em(15.0), w, type_size::XS, Weight::Regular, t.danger_fg());
                }
                if renaming {
                    let fy = y + em(96.0);
                    let (pg, f) = (page.clone(), rename_field.clone());
                    pergola::widgets::TextField::new(rename_field.clone())
                        .placeholder("A new name")
                        .at(em(16.0), fy)
                        .width(em(240.0))
                        .on_submit(move |v| {
                            let v = v.to_string();
                            with_printers(&pg, |a| a.rename(id, &v));
                            f.set(Default::default());
                        })
                        .render(ctx);
                    let (pg, f) = (page.clone(), rename_field.clone());
                    button(ctx, em(16.0 + 2.0 * SLOT + 8.0), fy + em(2.0), "Save", Look::Primary, true, move || {
                        let v = f.get_cloned().text().to_string();
                        with_printers(&pg, |a| a.rename(id, &v));
                        f.set(Default::default());
                    });
                    let pg = page.clone();
                    button(ctx, em(16.0 + 3.0 * SLOT), fy + em(2.0), "Cancel", Look::Ghost, true, move || with_printers(&pg, |a| a.renaming = None));
                }
                y += rh + em(8.0);
            }

            // Add a printer: by address, or one found nearby.
            y += em(12.0);
            text(ctx, 0.0, y + em(14.0), "Add a printer", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(36.0);
            let (pg, f) = (page.clone(), add_field.clone());
            pergola::widgets::TextField::new(add_field.clone())
                .placeholder("Address, like printer.local or ipps://10.0.0.5/ipp/print")
                .at(0.0, y)
                .width(em(360.0))
                .on_submit(move |v| {
                    let v = v.to_string();
                    with_printers(&pg, |a| a.add(&v));
                    f.set(Default::default());
                })
                .render(ctx);
            let (pg, f) = (page.clone(), add_field.clone());
            button(ctx, em(3.0 * SLOT + 8.0), y + em(2.0), "Add", Look::Primary, !p.busy, move || {
                let v = f.get_cloned().text().to_string();
                with_printers(&pg, |a| a.add(&v));
                f.set(Default::default());
            });
            y += em(44.0);
            let pg = page.clone();
            button(ctx, 0.0, y, if p.looking { "Looking…" } else { "Find printers nearby" }, Look::Secondary, !p.looking, move || with_printers(&pg, |a| a.look_nearby()));
            y += em(40.0);
            match &p.nearby {
                Some(Err(why)) => {
                    text(ctx, 0.0, y + em(10.0), &format!("Can't look: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "No printers found on this network.", type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                Some(Ok(v)) => {
                    for n in v {
                        let nh = em(48.0);
                        text(ctx, em(16.0), y + em(16.0), &n.name, type_size::SM, Weight::Medium, t.text_primary());
                        let sub = format!("{}{}", n.uri, if n.tls { "" } else { " \u{b7} not encrypted" });
                        text(ctx, em(16.0), y + em(34.0), &sub, type_size::XS, Weight::Regular, t.text_secondary());
                        if n.saved.is_some() {
                            text(ctx, em(3.0 * SLOT + 16.0), y + em(24.0), "Saved", type_size::SM, Weight::Regular, t.text_tertiary());
                        } else {
                            let (pg, uri) = (page.clone(), n.uri.clone());
                            button(ctx, em(3.0 * SLOT + 8.0), y + em(9.0), "Add", Look::Secondary, !p.busy, move || with_printers(&pg, |a| a.add(&uri)));
                        }
                        y += nh;
                    }
                }
                None => {}
            }

            // History (§6.5): the last 30 days, newest first.
            y += em(20.0);
            text(ctx, 0.0, y + em(14.0), "History", type_size::LG, Weight::Semibold, t.text_primary());
            let has = matches!(&p.history, Some(Ok(v)) if !v.is_empty());
            let pg = page.clone();
            let lab = if p.confirm_clear { "Clear all history" } else { "Clear history…" };
            button(ctx, em(SLOT), y, lab, if p.confirm_clear { Look::Danger } else { Look::Ghost }, has && !p.busy, move || with_printers(&pg, |a| a.clear_history()));
            y += em(40.0);
            text(ctx, 0.0, y + em(8.0), "Titles, printers and times for 30 days; never what was printed.", type_size::SM, Weight::Regular, t.text_tertiary());
            y += em(24.0);
            match &p.history {
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "Nothing printed yet.", type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                Some(Ok(v)) => {
                    for e in v {
                        let eh = em(44.0);
                        text(ctx, em(16.0), y + em(14.0), &e.title, type_size::SM, Weight::Medium, t.text_primary());
                        let when = ago(e.at, now);
                        let ww = width(&when, type_size::XS, Weight::Regular);
                        text(ctx, pw - ww - em(12.0), y + em(14.0), &when, type_size::XS, Weight::Regular, t.text_tertiary());
                        let failed = e.result == Phase::Failed;
                        text(ctx, em(16.0), y + em(32.0), &result(e), type_size::XS, Weight::Regular, if failed { t.danger_fg() } else { t.text_secondary() });
                        y += eh;
                    }
                }
                Some(Err(why)) => {
                    text(ctx, 0.0, y + em(10.0), why, type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                }
                None => {}
            }
            y += em(16.0);
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.printers_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(97)
            .backdrop(t.bg_canvas())
            .emit(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_and_clear_ask_twice_and_one_change_at_a_time() {
        let mut p = PrintersPage::default();
        p.remove(3);
        assert!(p.take_wants().is_empty());
        assert_eq!(p.confirm_remove, Some(3));
        p.remove(3);
        assert_eq!(p.take_wants(), vec![Want::Remove(3)]);
        p.identify(3);
        assert!(p.take_wants().is_empty(), "busy");
        p.apply(Got::Done(Ok("removed Office".into())));
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        assert_eq!(p.note, Some((false, "removed Office".into())));
        p.clear_history();
        p.clear_history();
        assert_eq!(p.take_wants(), vec![Want::ClearHistory]);
        p.apply(Got::Done(Err("prelumd is not running".into())));
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        p.rename(1, "  ");
        assert!(p.take_wants().is_empty(), "no empty names");
        p.rename(1, "Hall");
        assert_eq!(p.take_wants(), vec![Want::Rename(1, "Hall".into())]);
    }

    #[test]
    fn words() {
        let e = Ended { job: 1, title: "Q3".into(), printer: "Office".into(), sheets: 1, at: 0, result: Phase::Done, message: String::new(), persona: "Work".into() };
        assert_eq!(result(&e), "Printed, 1 sheet \u{b7} Office \u{b7} Work");
        assert_eq!(ago(0, 7200), "2 h ago");
    }
}
