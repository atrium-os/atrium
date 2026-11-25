//! navigator-settings: the Navigator's own pane in Settings (curia.md §15,
//! navigator-islands.md D8). It lists "Sites that run live", the origins a
//! person chose with "Always for this site", newest first, and lets them
//! take any or all of them back.
//!
//! limend starts it for Settings (`REQUEST_APP_SETTINGS`), and portcullisd
//! runs it as the Navigator's own uid in the persona Settings named, with
//! that persona's Navigator data at `ATRIUM_PANE_DATA` and nothing else of
//! the Navigator's. It reads the store directory from the bundle's
//! `etc/navigatord.conf` (`NAVIGATORD_STORE`), as navigatord does, and
//! changes the list through the same crate, so a revoke here reaches every
//! Navigator window of the persona at once. A revoke takes something away,
//! so it needs no press (curia.md §15).
//!
//! Logs `navigator-settings: N sites`, `navigator-settings: ask again for
//! ORIGIN`, `navigator-settings: ask again for every site`.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::proto::ChildEvent;
use navigator_live_origins::Remembered;
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::window::StepResult;
use pergola::{App, Color, Mutable, SizeSpec, TextStyle, Window, WindowDesc};

const W: f32 = 560.0;
const H: f32 = 420.0;
const ROW: f32 = 40.0;
const TOP: f32 = 104.0;
/// Rows on a page; the rest are a "Newer" or "Older" away.
const PER_PAGE: usize = 6;
/// How often the list is read again, for a change made in a Navigator.
const REREAD: Duration = Duration::from_secs(2);

/// The list's file: `NAVIGATORD_STORE` from navigatord's settings (later
/// lines win; blank is unset), an absolute path in the Navigator's jail,
/// found under `data`, where the pane sees that jail's own files. `None`
/// for no store (navigatord keeps the list in memory) or a path that
/// would leave `data`.
fn list_file(conf: &str, data: &Path) -> Option<PathBuf> {
    let store = conf.lines().map(str::trim).filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == "NAVIGATORD_STORE")
        .map(|(_, v)| v.trim()).last().filter(|v| !v.is_empty())?;
    let store = Path::new(store);
    let mut parts = store.components();
    if parts.next() != Some(Component::RootDir) || !parts.all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(data.join(store.strip_prefix("/").ok()?).join("live-origins"))
}

/// What the pane shows: an origin without its `https://`.
fn shown(origin: &str) -> &str {
    origin.strip_prefix("https://").unwrap_or(origin)
}

#[derive(Clone, Default)]
struct State {
    /// Newest first.
    list: Vec<String>,
    page: usize,
    /// No list to show: why.
    none: Option<String>,
}

#[derive(Clone)]
enum Ask {
    One(String),
    All,
}

struct Pane {
    state: Mutable<State>,
    asked: Mutable<Option<Ask>>,
}

fn style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

impl Pane {
    fn button(&self, ctx: &mut Ctx, rect: Rect, label: &str, ask: Option<Ask>, page: Option<usize>) {
        let t = ctx.theme;
        let b = ctx.push(Node::Rect { rect, fill: t.bg_surface(), radius: 6.0 });
        let (asked, state) = (self.asked.clone(), self.state.clone());
        ctx.on_click(b, move || {
            if let Some(a) = &ask { asked.set(Some(a.clone())); }
            if let Some(p) = page { let mut s = state.get_cloned(); s.page = p; state.set(s); }
        });
        ctx.add(Node::Text { rect: Rect::new(rect.x() + 12.0, rect.y() + 7.0, 0.0, 18.0), content: label.into(),
                             style: style(type_size::SM, Weight::Medium, t.text_primary()) });
        ctx.pop();
    }
}

impl View for Pane {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.state.get_cloned();
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, W, H), fill: t.bg_elevated(), radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(24.0, 20.0, 0.0, 24.0), content: "Sites that run live".into(),
                             style: style(type_size::LG, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, 52.0, 0.0, 18.0),
                             content: "These sites run their own code in Navigator without asking.".into(),
                             style: style(type_size::SM, Weight::Regular, t.text_secondary()) });
        ctx.add(Node::Text { rect: Rect::new(24.0, 72.0, 0.0, 18.0),
                             content: "Navigator asks again before running a site you remove.".into(),
                             style: style(type_size::SM, Weight::Regular, t.text_secondary()) });
        if let Some(why) = &s.none {
            ctx.add(Node::Text { rect: Rect::new(24.0, TOP + 8.0, 0.0, 20.0), content: why.clone(),
                                 style: style(type_size::MD, Weight::Regular, t.text_primary()) });
        } else if s.list.is_empty() {
            ctx.add(Node::Text { rect: Rect::new(24.0, TOP + 8.0, 0.0, 20.0), content: "No site runs live without asking.".into(),
                                 style: style(type_size::MD, Weight::Regular, t.text_primary()) });
        } else {
            let pages = s.list.len().div_ceil(PER_PAGE);
            let page = s.page.min(pages - 1);
            for (i, o) in s.list.iter().skip(page * PER_PAGE).take(PER_PAGE).enumerate() {
                let y = TOP + i as f32 * ROW;
                ctx.add(Node::Rect { rect: Rect::new(24.0, y + ROW - 1.0, W - 48.0, 1.0), fill: t.border_default(), radius: 0.0 });
                ctx.add(Node::Text { rect: Rect::new(24.0, y + 10.0, 0.0, 20.0), content: shown(o).into(),
                                     style: style(type_size::MD, Weight::Regular, t.text_primary()) });
                self.button(ctx, Rect::new(W - 24.0 - 96.0, y + 4.0, 96.0, 32.0), "Ask again", Some(Ask::One(o.clone())), None);
            }
            let foot = TOP + PER_PAGE as f32 * ROW + 16.0;
            self.button(ctx, Rect::new(24.0, foot, 190.0, 32.0), "Ask again for every site", Some(Ask::All), None);
            if pages > 1 {
                let first = page * PER_PAGE + 1;
                let last = (first + PER_PAGE - 1).min(s.list.len());
                ctx.add(Node::Text { rect: Rect::new(W - 24.0 - 200.0 - 96.0, foot + 7.0, 0.0, 18.0),
                                     content: format!("{first}–{last} of {}", s.list.len()),
                                     style: style(type_size::SM, Weight::Regular, t.text_secondary()) });
                if page > 0 {
                    self.button(ctx, Rect::new(W - 24.0 - 180.0, foot, 84.0, 32.0), "Newer", None, Some(page - 1));
                }
                if page + 1 < pages {
                    self.button(ctx, Rect::new(W - 24.0 - 84.0, foot, 84.0, 32.0), "Older", None, Some(page + 1));
                }
            }
        }
        ctx.pop();
    }
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("navigator-settings: {why}");
    std::process::exit(1)
}

/// The list, or why there is none to show.
fn read(list: &Option<Remembered>) -> Result<Vec<String>, String> {
    match list {
        Some(r) => Ok(r.list()),
        None => Err("Navigator keeps this list only while it runs.".into()),
    }
}

fn main() {
    let (mut child, fresco) = limen::client::Child::embedded().unwrap_or_else(|e| die(format_args!("no limend channel: {e:?}")));
    let embed = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, .. })) if role == "app-settings" => break embed,
            Ok(Some(ChildEvent::Attach { role, .. })) => die(format_args!("asked to be {role}")),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => die("limend never attached us"),
        }
    };
    let data = std::env::var_os("ATRIUM_PANE_DATA").map(PathBuf::from).unwrap_or_else(|| die("no ATRIUM_PANE_DATA"));
    // The bundle is the jail's root: navigatord.conf is beside bin/.
    let conf = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.join("etc/navigatord.conf")))
        .and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let mut list = list_file(&conf, &data).map(|f| Remembered::new(Some(f)));
    let conn = fresco_client::Connection::wrap(fresco).unwrap_or_else(|e| die(format_args!("frescod: {e}")));
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() },
        ..WindowDesc::new("Navigator", SizeSpec::Fixed(W as u32, H as u32))
    };
    let mut win = Window::open_on(conn, &desc).unwrap_or_else(|e| die(format_args!("window: {e}")));
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(State::default(), dirty.clone());
    let asked = Mutable::with_dirty(None, dirty.clone());
    let mut app = App::new_with_flag(Pane { state: state.clone(), asked: asked.clone() }, dirty);
    let mut ready = false;
    let started = Instant::now();
    let mut last_read: Option<Instant> = None;
    let mut last_count = None;
    loop {
        match win.step(&mut app, Some(Duration::from_millis(30))) {
            Ok(StepResult::Closed) | Err(_) => break,
            _ => {}
        }
        if !ready {
            if let Some(token) = win.embed_token() {
                ready = true;
                if child.ready(embed, token).is_err() {
                    die("limend refused ready");
                }
            } else if started.elapsed() > Duration::from_secs(10) {
                die("no embed token from frescod");
            }
        }
        match child.poll(Duration::from_millis(1)) {
            Ok(Some(ChildEvent::Detaching { .. })) | Err(_) => break,
            _ => {}
        }
        if let Some(a) = asked.get_cloned() {
            asked.set(None);
            if let Some(r) = list.as_mut() {
                let done = match &a {
                    Ask::One(o) => { eprintln!("navigator-settings: ask again for {o}"); r.forget(o) }
                    Ask::All => { eprintln!("navigator-settings: ask again for every site"); r.forget_all() }
                };
                if let Err(e) = done { eprintln!("navigator-settings: {e}") }
            }
            last_read = None;
        }
        if last_read.is_none_or(|t| t.elapsed() >= REREAD) {
            last_read = Some(Instant::now());
            let mut s = state.get_cloned();
            match read(&list) {
                Ok(l) => {
                    if last_count != Some(l.len()) { eprintln!("navigator-settings: {} sites", l.len()) }
                    last_count = Some(l.len());
                    if l != s.list || s.none.is_some() {
                        s.list = l;
                        s.none = None;
                        state.set(s);
                    }
                }
                Err(why) if s.none.as_ref() != Some(&why) => { s.none = Some(why); state.set(s) }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_where_navigatord_keeps_it_under_the_apps_data() {
        let data = Path::new("/app-data");
        let at = |conf: &str| list_file(conf, data);
        assert_eq!(at("NAVIGATORD_STORE=/var/db/navigatord/store\n"), Some(PathBuf::from("/app-data/var/db/navigatord/store/live-origins")));
        assert_eq!(at("# c\nNAVIGATORD_STORE = /a\nNAVIGATORD_STORE=/b\n"), Some(PathBuf::from("/app-data/b/live-origins")), "the later line");
        assert_eq!(at("NAVIGATORD_STORE=\n"), None, "blank is unset: kept in memory");
        assert_eq!(at("NAVIGATORD_PRERENDER=/x\n"), None);
        assert_eq!(at("NAVIGATORD_STORE=relative/store\n"), None);
        assert_eq!(at("NAVIGATORD_STORE=/var/../../etc\n"), None, "nothing outside the app's data");
        assert_eq!(at("NAVIGATORD_STORE=/var/./db\n").is_some(), true, "a `.` std drops is harmless");
    }

    #[test]
    fn a_site_is_shown_without_its_scheme() {
        assert_eq!(shown("https://app.example:8443"), "app.example:8443");
    }
}
