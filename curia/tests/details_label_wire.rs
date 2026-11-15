//! Curia's Network page through pergola's real FrescoSurface into
//! fresco-scene-server's frontend (frescod's scene state), over a socket
//! pair: what frescod holds after each frame, not what the deltas say.
//!
//! Seen live: "Show details" drawn twice, 4 pt apart, after the saved
//! networks arrived in the tick the WM's size did. A resize resends every
//! node (b9ccaf6d), and that resend diffed from an empty tree, so the label
//! the last frame sent, under another id now, was never removed.
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use curia::network::*;
use curia::view::*;
use pergola::Mutable;
use atrium_netconf::{Link, Online, Profile, Status};

#[test]
fn the_details_label_is_held_once_after_a_resize_and_a_change_in_one_tick() {
    std::env::set_var("ATRIUM_FONT_PATH", concat!(env!("CARGO_MANIFEST_DIR"), "/../fonts/ibm-plex:", env!("CARGO_MANIFEST_DIR"), "/../fonts/liberation:", env!("CARGO_MANIFEST_DIR"), "/../fonts/phosphor"));
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    let texts: Arc<Mutex<HashMap<u32, String>>> = Default::default();
    let fe = Arc::new(Mutex::new({
        use fresco_scene_server::cas::store::CasStore;
        let cas = Arc::new(Mutex::new(CasStore::new()));
        let scene = Arc::new(Mutex::new(fresco_scene_server::scene::graph::SceneGraph::new()));
        let slots = Arc::new(Mutex::new(fresco_scene_server::scene::slots::SlotTable::new()));
        let comp = Arc::new(Mutex::new(fresco_scene_server::window::Compositor::new_with_window0(scene, slots)));
        fresco_scene_server::command::envelope_frontend::EnvelopeFrontend::new(cas, comp)
    }));
    let frames = Arc::new((Mutex::new(0u32), std::sync::Condvar::new()));
    {
        let (fe, texts, frames) = (fe.clone(), texts.clone(), frames.clone());
        std::thread::spawn(move || {
            let mut c = aqueduct::Connection::wrap(b).unwrap();
            loop {
                let Ok(m) = c.recv_message() else { return };
                if m.op == fresco_protocol::control::OP_TEXT_RUN_INSTALL {
                    let p: fresco_protocol::TextRunInstallPayload = fresco_protocol::decode(&m.payload).unwrap();
                    texts.lock().unwrap().insert(p.node_id, p.text.clone());
                }
                let outs = match fe.lock().unwrap().dispatch(&m, 1) {
                    Ok(v) => v,
                    Err(e) => { eprintln!("op {:#x}: {e:?}", m.op); Vec::new() }
                };
                for o in outs {
                    c.send_message(aqueduct::classes::CLASS_DISPLAY, o.op, o.flags, &o.payload).unwrap();
                }
                if m.op == fresco_protocol::control::OP_SCENE_FRAME_END {
                    let (l, cv) = &*frames;
                    *l.lock().unwrap() += 1;
                    cv.notify_all();
                }
            }
        });
    }
    let conn = fresco_client::Connection::wrap(a).unwrap();
    let mut surface = pergola::surface::FrescoSurface::new(conn, 0);
    let wait = |n: u32| {
        let (l, cv) = &*frames;
        let mut g = l.lock().unwrap();
        while *g < n { g = cv.wait(g).unwrap(); }
    };
    let held = || {
        let f = fe.lock().unwrap();
        let s = f.window_state(0).unwrap();
        let t = texts.lock().unwrap();
        let mut v: Vec<(u32, String, Option<u32>)> = s.glyph_run_nodes.keys()
            .filter_map(|id| t.get(id).filter(|s| s.ends_with(" details")).map(|t| (*id, t.clone(), s_parent(s_pl(&s.placements, *id)))))
            .collect();
        v.sort();
        v
    };
    fn s_pl(p: &HashMap<u32, fresco_protocol::PlaceParams>, id: u32) -> Option<fresco_protocol::PlaceParams> { p.get(&id).cloned() }
    fn s_parent(p: Option<fresco_protocol::PlaceParams>) -> Option<u32> { p.and_then(|p| p.parent) }

    let dirty = Arc::new(AtomicBool::new(true));
    macro_rules! m { ($v:expr) => { Mutable::with_dirty($v, dirty.clone()) }; }
    let mut p = NetPage::new();
    p.take_wants();
    p.apply(Got::Status(Some(Status { link: Link::Wifi, online: Online::Unknown, metered: false, bars: Some(3), ssid: Some(b"aura-test".to_vec()), portal: None })));
    let page = m!(p.clone());
    let cred = pergola::widgets::CredentialField::wifi_passphrase(Mutable::new(Default::default()));
    let v = CuriaView { which: m!(Which::Network), page: page.clone(), devs: m!(Default::default()), dev_scroll: m!(Default::default()), disp: m!(Default::default()), disp_scroll: m!(Default::default()), sound: m!(Default::default()), sound_scroll: m!(Default::default()), apps: m!(Default::default()), apps_scroll: m!(Default::default()), bt: m!(Default::default()), bt_scroll: m!(Default::default()), bt_code: m!(Default::default()), net_fields: std::array::from_fn(|_| m!(Default::default())), net_scroll: m!(Default::default()), cred, size: m!((1080.0, 682.0)) };
    let mut app = pergola::App::new_with_flag(v, dirty.clone());
    pergola::commit(&mut surface, &app.tick()).unwrap();
    wait(1);
    assert_eq!(held().len(), 1);
    p.apply(Got::Profiles(Ok(vec![Profile { ssid: b"aura-test".to_vec(), open: false, wpa3_only: false, mac: "network".into(), auto: true, prio: 0, metered: false, manual: None }])));
    page.set(p.clone());
    // The WM's size arrives in the same tick (pergola resends on resize).
    app.resend_all();
    pergola::commit(&mut surface, &app.tick()).unwrap();
    wait(2);
    assert_eq!(held().len(), 1, "{:?}", held());
    for i in 0..3 {
        pergola::commit(&mut surface, &app.tick()).unwrap();
        wait(3 + i);
    }
    let h = held();
    assert_eq!(h.len(), 1, "{h:?}");
}
