//! frescod-vulkan-smoke — end-to-end integration test of the new stack.
//!
//! Wires every M2 piece together in one process:
//!
//!   aqueduct::Connection (envelope transport)
//!   ↓
//!   fresco_scene_server::EnvelopeFrontend (per-window scene state)
//!   ↓ (extract_rect_nodes / extract_texture_batches)
//!   fresco_vulkan::HeadlessRenderer (compute + indirect draw)
//!   ↓ (read_pixels)
//!   PNG dump to disk
//!
//! No scanout integration (atrium-gpu-rs page-flip lands at M2.7d
//! final). No multi-app fan-out (per-connection writer threads land
//! when the production main.rs rewrite happens). Single-client,
//! single-window, frame-N PNG dump for behavioral validation.
//!
//! Usage:
//!   FRESCOD_SOCK=/tmp/frescod-smoke.sock cargo run --bin frescod-vulkan-smoke --features vulkan
//!   atrium-test-client /tmp/frescod-smoke.sock
//!   ls /tmp/frescod-smoke-frame-*.png
//!
//! Each scene-frame-end commits a render + a PNG. Useful for proving
//! the pipeline works before M2.7d's full main.rs rewrite.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aqueduct::{Connection as AqConn, MessageKind, CLASS_DISPLAY};
use fresco_protocol::control;
use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_scene_server::scene::graph::SceneGraph;
use fresco_scene_server::scene::slots::SlotTable;
use fresco_scene_server::window::Compositor;
use fresco_vulkan::HeadlessRenderer;

const WIDTH:  u32 = 1280;
const HEIGHT: u32 = 720;
const SOCK_DEFAULT:    &str = "/tmp/frescod-smoke.sock";
const PNG_PREFIX_DEFAULT: &str = "/tmp/frescod-smoke-frame-";

fn main() -> io::Result<()> {
    let _ = env_logger::try_init();

    let sock_path = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| SOCK_DEFAULT.to_string());
    let png_prefix = std::env::var("FRESCOD_SMOKE_PNG")
        .unwrap_or_else(|_| PNG_PREFIX_DEFAULT.to_string());

    /* Stale-socket cleanup (frescod conventionally rebinds on
     * restart). */
    let _ = std::fs::remove_file(&sock_path);
    let listener = std::os::unix::net::UnixListener::bind(&sock_path)?;
    eprintln!("frescod-vulkan-smoke: listening on {sock_path}");
    eprintln!("frescod-vulkan-smoke: render target {WIDTH}×{HEIGHT}");
    eprintln!("frescod-vulkan-smoke: PNGs → {png_prefix}<N>.png on each SCENE_FRAME_END");

    /* Initialize the renderer + load the bundle. Single global instance
     * for this smoke harness; production frescod has one per display
     * output. */
    let mut renderer = HeadlessRenderer::new(WIDTH, HEIGHT)
        .map_err(|e| io::Error::new(io::ErrorKind::Other,
            format!("HeadlessRenderer::new: {e}")))?;
    let bundle_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .join("bundles/atrium-core");
    if !bundle_path.join("compute/op_rectangle.comp.spv").exists() {
        eprintln!("error: SPIR-V not built. Run bundles/atrium-core/build.sh first.");
        return Err(io::Error::new(io::ErrorKind::NotFound, "missing SPIR-V"));
    }
    renderer.load_bundle(&bundle_path)
        .map_err(|e| io::Error::new(io::ErrorKind::Other,
            format!("load_bundle: {e}")))?;
    eprintln!("frescod-vulkan-smoke: atrium-core bundle loaded ({} ops)",
        renderer.op_count());

    /* Set up the dispatcher. Compositor + CasStore + SceneGraph + SlotTable
     * have to be wired even though we don't use most of them; the
     * EnvelopeFrontend shares them with what would normally be the legacy
     * frontend during transition. */
    let cas   = Arc::new(Mutex::new(CasStore::new()));
    let scene = Arc::new(Mutex::new(SceneGraph::new()));
    let slots = Arc::new(Mutex::new(SlotTable::new()));
    let comp  = Arc::new(Mutex::new(Compositor::new_with_window0(scene, slots)));
    let mut frontend = EnvelopeFrontend::new(cas, comp);

    /* Single-client smoke loop: accept one connection, drive everything
     * inline. Multi-client fan-out is M2.7d-final. */
    eprintln!("frescod-vulkan-smoke: waiting for client...");
    let (stream, _) = listener.accept()?;
    eprintln!("frescod-vulkan-smoke: client connected");

    let mut conn = AqConn::wrap(stream)?;
    let mut frame_no: u32 = 0;
    let client_id: u8 = 1;

    loop {
        let msg = match conn.recv_message() {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                eprintln!("frescod-vulkan-smoke: client disconnected");
                break;
            }
            Err(e) => return Err(e),
        };

        if msg.opcode_class != CLASS_DISPLAY {
            log::debug!("ignored class={} op={:#x}", msg.opcode_class, msg.op);
            continue;
        }

        let was_frame_end = msg.op == control::OP_SCENE_FRAME_END;

        let outbound = match frontend.dispatch(&msg, client_id) {
            Ok(o)  => o,
            Err(e) => {
                eprintln!("dispatch op={:#x}: {e:?}", msg.op);
                Vec::new()
            }
        };

        /* Send any responses back over the wire (e.g. WINDOW_CREATE's
         * IS_RESPONSE with the assigned window_id). */
        for o in outbound {
            conn.send_message(CLASS_DISPLAY, o.op, o.flags, &o.payload)?;
        }

        /* On SCENE_FRAME_END, render + dump. */
        if was_frame_end {
            let win_id = msg.flags as u32;
            if let Some(state) = frontend.window_state(win_id) {
                let rects    = state.extract_rect_nodes();
                let textures = state.extract_texture_batches();
                renderer.set_rect_nodes(rects);
                renderer.set_texture_batches(textures);
                renderer.render_to_buffer()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("render: {e}")))?;
                let pixels = renderer.read_pixels_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("readback: {e}")))?;

                /* HeadlessRenderer's color format is BGRA8_UNORM; the
                 * `image` crate wants RGBA, so swap channels. */
                let mut rgba = pixels.clone();
                for px in rgba.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }

                let path = format!("{png_prefix}{frame_no:04}.png");
                let img = image::RgbaImage::from_raw(WIDTH, HEIGHT, rgba)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other,
                        "RgbaImage::from_raw"))?;
                img.save(&path)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("save PNG: {e}")))?;
                eprintln!("frescod-vulkan-smoke: frame {frame_no} → {path}");
                frame_no += 1;
            }
        }

        let _ = MessageKind::Request;  // pacify import-unused on some configs
    }

    Ok(())
}
