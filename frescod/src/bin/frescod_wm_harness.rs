//! frescod-wm-harness — a headless frescod for exercising the F0 window-management
//! path over a real socket, without the GPU/Vulkan/display stack.
//!
//! The full `frescod` binary opens /dev/atrium-gpu0 + the display and runs a Vulkan
//! render loop — none of which the WM protocol needs. This harness wires up exactly
//! the parts the host-side F0 integration test could NOT cover: the real
//! `socket_server` (with the FORUM_WM_UID capability admission over LOCAL_PEERCRED)
//! and the real `EnvelopeFrontend` (the OP_WM_* handlers), bound to a Unix socket.
//! So the forum-wm binary + ordinary app clients can connect over the wire and the
//! enumerate → arrange → declare → render-gate loop runs end-to-end in the VM.
//!
//! Set FRESCOD_SOCK to choose the socket (default /tmp/frescod.sock) and
//! FORUM_WM_UID to the uid that should be granted window-management.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;

#[path = "../laminar.rs"]
mod laminar;
#[path = "../socket_server.rs"]
mod socket_server;

fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    let frontend = Arc::new(Mutex::new(EnvelopeFrontend::new(cas, comp)));

    let sock = std::env::var("FRESCOD_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/frescod.sock"));

    // No deadline lane here — the WM path doesn't use it.
    let _subs = socket_server::spawn(
        socket_server::Shared { frontend, lane: None },
        &sock,
        None,
    )?;

    eprintln!(
        "frescod-wm-harness: listening on {} (FORUM_WM_UID={})",
        sock.display(),
        std::env::var("FORUM_WM_UID").unwrap_or_else(|_| "<unset>".into()),
    );

    // Park; the socket server runs on its own threads.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
