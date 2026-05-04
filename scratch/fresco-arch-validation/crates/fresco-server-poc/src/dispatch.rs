//! Server-side dispatcher: accepts atrium-rpc connections, decodes
//! display-class messages, mutates the in-memory `SceneState`.
//!
//! Step 5b scope: pure host-side state machine. No GPU writes yet —
//! the renderer keeps clearing to teal. The dispatcher's job is
//! "received the wire bytes, decoded them, recorded the intent."
//! Steps 6-8 layer host CAS + GPU buffer writes + actual rendering
//! on top.
//!
//! Threading: one listener thread accepts connections; each
//! connection runs in its own thread (single-tenant for POC, but
//! the multi-thread shape is correct for multi-app later). All
//! threads share `Arc<Mutex<SceneState>>`. The main thread (winit
//! event loop + Vulkan renderer) doesn't touch SceneState yet —
//! step 7 wires the GPU read.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use ash::vk;
use atrium_rpc::{Connection, Message};
use atrium_rpc_display::{control, decode, scene_ops, CLASS_DISPLAY,
    SceneNodeClearPayload, SceneNodeSetPayload, SlotClearPayload, SlotKind,
    SlotSetPayload, TextureFormat};
use fresco_vulkan::UploadRequest;

/// Default UDS path. macOS has no `/atrium/sockets/`; use /tmp for
/// dev. The env var override is the standard escape hatch.
const DEFAULT_SOCKET: &str = "/tmp/fresco-poc.sock";

pub fn socket_path() -> PathBuf {
    std::env::var_os("FRESCO_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

/// Per-connection scene state. One per connected client.
///
/// Step 5b: just records what the client sent. Step 6 turns
/// `slot_table` into actual `vkImage` resources via the host CAS.
/// Step 7 turns `nodes` into entries in the GPU scene buffer.
#[derive(Default)]
pub struct SceneState {
    /// slot_id → CAS hash. Set by SLOT_SET; cleared by SLOT_CLEAR.
    pub slot_table: HashMap<u32, [u8; 32]>,

    /// node_id → (op_id, raw param bytes from SCENE_NODE_SET).
    /// Param bytes are postcard-encoded per the op's schema; the
    /// host shim decodes them when it copies into the GPU scene
    /// buffer at step 7.
    pub nodes: HashMap<u32, (u32, Vec<u8>)>,

    /// Number of frames the client has committed. Increments on
    /// each SCENE_FRAME_END. Useful for logging + a future
    /// "wait until N frames committed" handshake.
    pub frames_committed: u64,

    /// Step 6 cross-thread queue: dispatcher (this thread) decodes
    /// SLOT_SET, looks up the bytes in atrium-rpc's CAS, and pushes
    /// an UploadRequest. The renderer drains on the main thread
    /// before each frame because Vulkan resource ops must run on the
    /// device-owning thread.
    pub pending_uploads: Vec<UploadRequest>,
    pub pending_clears:  Vec<u32>,
}

impl SceneState {
    pub fn new() -> Self { Self::default() }
}

/// Spawn the listener thread and return immediately. The thread
/// loops accepting connections; each connection runs in its own
/// thread.
pub fn spawn_listener(state: Arc<Mutex<SceneState>>) -> Result<()> {
    let path = socket_path();
    /* Stale-socket cleanup: if a previous server crashed mid-bind,
     * the file is still on disk and bind() refuses to overwrite. */
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .with_context(|| format!("bind UDS {}", path.display()))?;
    log::info!("dispatcher listening on {}", path.display());

    thread::Builder::new()
        .name("fresco-listener".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        let st = Arc::clone(&state);
                        thread::Builder::new()
                            .name("fresco-conn".into())
                            .spawn(move || {
                                if let Err(e) = serve_one(stream, st) {
                                    log::warn!("connection: {e:?}");
                                }
                            })
                            .ok();
                    }
                    Err(e) => log::warn!("accept: {e:?}"),
                }
            }
        })
        .context("spawn listener thread")?;
    Ok(())
}

fn serve_one(
    stream: std::os::unix::net::UnixStream,
    state:  Arc<Mutex<SceneState>>,
) -> Result<()> {
    let mut conn = Connection::wrap(stream).context("Connection::wrap")?;
    log::info!("client connected");
    loop {
        let msg = match conn.recv_message() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                log::info!("client disconnected");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if msg.opcode_class != CLASS_DISPLAY {
            log::debug!("ignored non-display message: class={} op={:#x}",
                msg.opcode_class, msg.op);
            continue;
        }
        if let Err(e) = handle(&mut conn, &state, &msg) {
            log::warn!("dispatch op {:#x}: {e:?}", msg.op);
        }
    }
}

fn handle(
    conn:  &mut Connection,
    state: &Arc<Mutex<SceneState>>,
    msg:   &Message,
) -> Result<()> {
    match msg.op {
        control::OP_SLOT_SET => {
            let p: SlotSetPayload = decode(&msg.payload)?;
            /* Bytes must already be in atrium-rpc's CAS via a prior
             * CLASS_CORE upload. If not, we'd FETCH_REQUEST in a real
             * server; the POC just rejects. */
            let bytes = conn.cache_get(&p.hash)
                .ok_or_else(|| anyhow::anyhow!(
                    "SLOT_SET slot={} hash {}.. not in CAS",
                    p.slot_id, hex8(&p.hash)))?
                .to_vec();
            log::info!("SLOT_SET slot={} hash={}.. {}B",
                p.slot_id, hex8(&p.hash), bytes.len());
            let req = match p.kind {
                SlotKind::Texture(d) => UploadRequest::Texture {
                    slot_id: p.slot_id,
                    bytes,
                    width:   d.width,
                    height:  d.height,
                    format:  texture_format_to_vk(d.format),
                },
            };
            let mut s = state.lock().unwrap();
            s.slot_table.insert(p.slot_id, p.hash);
            s.pending_uploads.push(req);
        }
        control::OP_SLOT_CLEAR => {
            let p: SlotClearPayload = decode(&msg.payload)?;
            log::info!("SLOT_CLEAR slot={}", p.slot_id);
            let mut s = state.lock().unwrap();
            s.slot_table.remove(&p.slot_id);
            s.pending_clears.push(p.slot_id);
        }
        control::OP_SCENE_FRAME_BEGIN => {
            log::debug!("SCENE_FRAME_BEGIN");
        }
        control::OP_SCENE_FRAME_END => {
            let mut s = state.lock().unwrap();
            s.frames_committed += 1;
            log::info!("SCENE_FRAME_END (frame #{}, {} nodes, {} slots)",
                s.frames_committed, s.nodes.len(), s.slot_table.len());
        }
        control::OP_SCENE_NODE_SET => {
            let p: SceneNodeSetPayload = decode(&msg.payload)?;
            let op_name = scene_op_name(p.op_id);
            log::info!("SCENE_NODE_SET node={} op={:#x} ({}) params={}B",
                p.node_id, p.op_id, op_name, p.params.len());
            state.lock().unwrap().nodes.insert(p.node_id, (p.op_id, p.params));
        }
        control::OP_SCENE_NODE_CLEAR => {
            let p: SceneNodeClearPayload = decode(&msg.payload)?;
            log::info!("SCENE_NODE_CLEAR node={}", p.node_id);
            state.lock().unwrap().nodes.remove(&p.node_id);
        }
        op => {
            log::warn!("unhandled display op {:#x} (payload {}B)",
                op, msg.payload.len());
        }
    }
    Ok(())
}

fn scene_op_name(id: u32) -> &'static str {
    match id {
        scene_ops::ATRIUM_CORE_RECT    => "atrium-core.rect",
        scene_ops::ATRIUM_CORE_TEXTURE => "atrium-core.texture",
        scene_ops::ATRIUM_CORE_PATH    => "atrium-core.path",
        scene_ops::ATRIUM_CORE_GLYPH   => "atrium-core.glyph",
        _ => "<unknown>",
    }
}

fn texture_format_to_vk(f: TextureFormat) -> vk::Format {
    match f {
        TextureFormat::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
    }
}

fn hex8(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{:02x}", b)).collect()
}
