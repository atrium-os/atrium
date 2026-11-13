//! navigator-ui: the Navigator's window on a live frescod.
//!
//!   FRESCO_SOCKET=… navigator-ui [--scene main|prompt|start|live|stop] [--dark|--light]
//!
//! Build step 1 (docs/spec/navigator-ui.md §14): the stand-in trail store,
//! the page as a placeholder slot. `--scene` opens on one of §5.8's frames
//! (default: main, with the status bar off as D11 has it). Without
//! `--dark`/`--light` the window follows the system appearance.
//! `NAVIGATOR_UI_SECS=n` exits after n seconds (for VM screenshots);
//! `NAVIGATOR_UI_SIZE=WxH` sets the window's size (default 1280x696, the
//! 720-line screen under frescod's 24 px origin strip).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use navigator_ui::fake::Scene;
use navigator_ui::keys;
use navigator_ui::shell::{Shell, PAGE_SLOT};
use navigator_ui::ui::{now_ms, ShellView};

use pergola::theme::Semantic;
use pergola::{App, Event, Mutable, SizeSpec, Window, WindowDesc};

/// Where the page slot is, for the log, until Limen binds it (step 2).
struct LogBinder;

impl pergola::slot::SlotBinder for LogBinder {
    fn place(&mut self, window_id: u32, key: pergola::slot::SlotKey, rect: Option<[f32; 4]>) {
        if key == PAGE_SLOT {
            eprintln!("navigator-ui: page slot of window {window_id} -> {rect:?}");
        }
    }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut scene = None;
    let mut theme = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scene" => {
                i += 1;
                scene = args.get(i).and_then(|s| Scene::parse(s));
                if scene.is_none() {
                    eprintln!("navigator-ui: --scene main|prompt|start|live|stop");
                    std::process::exit(2);
                }
            }
            "--dark" => theme = Some(Semantic::DARK),
            "--light" => theme = Some(Semantic::LIGHT),
            a => {
                eprintln!("navigator-ui: unknown argument {a}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let (w, h) = std::env::var("NAVIGATOR_UI_SIZE").ok()
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((1280u32, 696u32));
    if let Some(secs) = std::env::var("NAVIGATOR_UI_SECS").ok().and_then(|s| s.parse::<u64>().ok()) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            std::process::exit(0);
        });
    }

    let mut window = Window::open(&WindowDesc::new("Navigator", SizeSpec::Fixed(w, h)))?;
    window.set_slot_binder(Box::new(LogBinder));
    window.follow_appearance(theme.is_none());
    let dirty = Arc::new(AtomicBool::new(true));
    let mut shell = match scene {
        Some(sc) => Shell::scene(sc, window.width, window.height),
        None => Shell::new(navigator_ui::fake::store(), window.width, window.height),
    };
    shell.w = window.width;
    shell.h = window.height;
    let state = Mutable::with_dirty(shell, dirty.clone());
    let mut app = App::new_with_flag(ShellView { state: state.clone() }, dirty);
    if let Some(t) = theme {
        app = app.with_theme(t);
    }

    window.run(&mut app, move |raw, _app, flow| {
        if let fresco_client::Event::Resized { width, height, .. } = raw {
            let mut s = state.get_cloned();
            s.w = *width as f32;
            s.h = *height as f32;
            state.set(s);
            return;
        }
        let Some(ev) = pergola::input::translate(raw) else { return };
        match &ev {
            Event::Key { modifiers, kind, key, chars } => {
                let mut s = state.get_cloned();
                s.mods = *modifiers;
                let now = now_ms();
                if let Some(a) = keys::action(&ev) {
                    s.apply(a, now);
                    flow.consume();
                } else if s.editing.is_some() && kind.pressed() {
                    if s.edit_key(*key, *modifiers, chars, now) {
                        flow.consume();
                    }
                } else if *key == pergola::Key::Escape && s.popover {
                    s.popover = false;
                    flow.consume();
                }
                if kind.pressed() {
                    navigator_ui::ui::trace(&mut s);
                }
                state.set(s);
            }
            Event::PointerDown { at } => {
                let mut s = state.get_cloned();
                s.press_at(at.x, at.y);
                state.set(s);
            }
            _ => {}
        }
    })
}
