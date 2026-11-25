//! Artifex's plugin host and this plugin through limend's own code (its
//! daemon on a real socket, a launcher that runs this program with its
//! Limen channel on stdin): the plugin is listed, started, told the file,
//! and its warnings, fix and command come back. No jail and no frescod:
//! the VM check covers those.

use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use artifex::lsp;
use artifex::plugins::{Host, LimenLink};
use limen::catalogue::SlotKind;
use limen::daemon::{Compositor, Launcher, Shared};
use limen::proto::{EmbedId, PluginInfo, WindowToken};

const APP: &str = "org.atrium.sample.md-links";

#[derive(Default)]
struct Fresco(Vec<UnixStream>);
impl Compositor for Fresco {
    fn bind(&mut self, _p: WindowToken, _c: WindowToken, _k: SlotKind, _r: Option<[f32; 4]>, _z: u8) -> Result<(u32, u32), u8> {
        Err(1)
    }
    fn embed_connection(&mut self, _owner: u32) -> Result<UnixStream, u8> {
        let (a, b) = UnixStream::pair().map_err(|_| 1u8)?;
        self.0.push(b);
        Ok(a)
    }
}

struct Run(Arc<Mutex<Vec<std::process::Child>>>);
impl Launcher for Run {
    fn launch(&mut self, app: &str, _embed: EmbedId, ch: UnixStream) -> Result<u32, String> {
        assert_eq!(app, APP);
        let c = std::process::Command::new(env!("CARGO_BIN_EXE_md-links")).stdin(OwnedFd::from(ch)).spawn().map_err(|e| e.to_string())?;
        self.0.lock().unwrap().push(c);
        Ok(4242)
    }
    fn end(&mut self, _embed: EmbedId) {}
}

fn until(h: &mut Host, what: &str, mut done: impl FnMut(&mut Host, Vec<lsp::Event>) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        let evs = h.pump().into_iter().map(|(_, e)| e).collect();
        if done(h, evs) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{what}: nothing in 10 s; stopped: {:?}", h.stopped);
}

#[test]
fn a_plugin_through_limend_marks_fixes_and_counts() {
    let d = std::env::temp_dir().join(format!("md-links-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let sock = d.join("limend.sock");
    let procs = Arc::new(Mutex::new(Vec::new()));
    let resolve = Box::new(|_uid: u32| Some(("ana".to_string(), "org.atrium.artifex".to_string())));
    let shared = Shared::new(Box::new(Fresco::default()), Box::new(Run(procs.clone())), resolve, Box::new(|_: &str, _: &str| None));
    shared.set_plugin_check(Box::new(|person: &str, app: &str| person == "ana" && app == APP));
    shared.set_plugin_list(Box::new(|_: &str| vec![PluginInfo { app: APP.into(), name: "Markdown links".into(), languages: vec!["markdown".into()],
        commands: vec!["md-links.check".into()], panel: None, workspace_read: false, network: vec![] }]));
    let l = UnixListener::bind(&sock).unwrap();
    let sh = shared.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let sh = sh.clone();
            std::thread::spawn(move || { let _ = limen::daemon::serve(aqueduct::Connection::wrap(s).unwrap(), 1001, sh); });
        }
    });
    std::env::set_var("LIMEN_SOCKET", &sock);

    let (link, _binder) = LimenLink::connect([1; 16]).unwrap();
    let mut h = Host::default();
    h.set_link(Box::new(link));
    h.refresh().unwrap();
    assert_eq!(h.installed.iter().map(|i| i.app.as_str()).collect::<Vec<_>>(), vec![APP], "listed by limend");
    h.reset(&d, "hello");
    h.switch(APP, true).unwrap();
    let file = d.join("README.md");
    h.open("markdown", &file, 1, "# Install it\n\nSee [the steps](#instal-it).\n");
    assert_eq!(h.runs.len(), 1, "started: {:?}", h.stopped);

    // Its warning, on the anchor.
    until(&mut h, "diagnostics", |h, _| h.runs[0].client.diagnostics.get(&file).is_some_and(|v| !v.is_empty()));
    let g = h.runs[0].client.diagnostics[&file][0].clone();
    assert_eq!((g.start, g.end, g.message.as_str()), ((2, 17), (2, 26), "No heading here has the anchor #instal-it"));
    // Its fix.
    h.runs[0].client.code_actions(&file, ((2, 0), (2, 30)));
    let mut got = None;
    until(&mut h, "quick fix", |_, evs| {
        got = evs.into_iter().find_map(|e| match e { lsp::Event::Actions(a) => Some(a), _ => None });
        got.is_some()
    });
    let a = got.unwrap();
    assert_eq!(a[0].title, "Link to #install-it");
    assert_eq!(a[0].edits[0].1[0].text, "install-it");
    // Its command, then the status it sets; a changed file is checked again.
    until(&mut h, "command", |h, _| !h.runs[0].commands.is_empty());
    h.command(APP, "md-links.check", Some(&file), 2).unwrap();
    until(&mut h, "status", |h, _| h.runs[0].status == "1 link goes nowhere");
    h.change(&file, 2, "# Install it\n\nSee [the steps](#install-it).\n");
    until(&mut h, "fixed", |h, _| h.runs[0].client.diagnostics.get(&file).is_some_and(|v| v.is_empty()));

    h.stop_all();
    for mut c in procs.lock().unwrap().drain(..) {
        let t = Instant::now();
        // Detached: it exits on its own.
        while c.try_wait().unwrap().is_none() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(c.try_wait().unwrap().is_some(), "the plugin exits when Artifex detaches it");
    }
    let _ = std::fs::remove_dir_all(Path::new(&d));
}
