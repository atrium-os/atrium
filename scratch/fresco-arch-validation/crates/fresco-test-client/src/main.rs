//! fresco-test-client — emits test scenes to fresco-server-poc.
//!
//! Step 5b: real wire dispatch. Connects to the server's Unix socket,
//! uploads any required CAS blobs (none for Scene A; the texture for
//! Scene B), then sends a frame of SCENE_NODE_SET messages bracketed
//! by SCENE_FRAME_BEGIN / SCENE_FRAME_END.
//!
//! Server-side state mutation is what's verified at step 5b — visual
//! output still pending until the GPU pipeline lands in steps 7-9.
//!
//! Usage:
//!   FRESCO_SOCKET=/tmp/fresco-poc.sock fresco-test-client scene-a
//!   FRESCO_SOCKET=/tmp/fresco-poc.sock fresco-test-client scene-b

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use atrium_rpc::Connection;
use atrium_rpc_display::{
    control, encode, scene_ops, CLASS_DISPLAY,
    RectParams, SceneFrameBeginPayload, SceneFrameEndPayload,
    SceneNodeSetPayload, SlotKind, SlotSetPayload, TextureDesc,
    TextureFormat, TextureParams,
};

const DEFAULT_SOCKET: &str = "/tmp/fresco-poc.sock";

fn socket_path() -> PathBuf {
    std::env::var_os("FRESCO_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

fn usage() -> ! {
    eprintln!("usage: fresco-test-client <scene-a|scene-b>");
    eprintln!("env: FRESCO_SOCKET (default {DEFAULT_SOCKET})");
    std::process::exit(2);
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 { usage(); }
    let scene = args[1].as_str();

    let path = socket_path();
    log::info!("connecting to {}", path.display());
    let mut conn = match Connection::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect {}: {e}", path.display());
            eprintln!("(is fresco-server-poc running?)");
            return ExitCode::from(1);
        }
    };

    let result = match scene {
        "scene-a" => scene_a(&mut conn),
        "scene-b" => scene_b(&mut conn),
        _ => usage(),
    };

    match result {
        Ok(()) => {
            log::info!("scene committed");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:?}");
            ExitCode::from(1)
        }
    }
}

/// Scene A: 1000 deterministic-random rects. No CAS resources.
/// Validates the rect-only path through the wire dispatcher.
fn scene_a(conn: &mut Connection) -> anyhow::Result<()> {
    /* 1000 rects = the original POC target. The compute kernel can
     * easily handle this (max_instances cap is 65 536); the wire
     * path is roughly 1000 × (10-byte envelope + 32-byte payload +
     * 8-byte SCENE_NODE_SET outer) ≈ 50 KiB per frame. */
    const N: u32 = 1000;
    log::info!("scene-a: {N} rects");

    let mut wire = 0;
    wire += send_ctrl(conn, control::OP_SCENE_FRAME_BEGIN, &SceneFrameBeginPayload::default())?;
    let mut rng = SmallRng::new(0xA77E_F0C0_5CE_AAAA);
    for i in 0..N {
        let x = rng.unit() * 1800.0;
        let y = rng.unit() * 1000.0;
        let w = 50.0 + rng.unit() * 200.0;
        let h = 50.0 + rng.unit() * 200.0;
        let r = rng.unit();
        let g = rng.unit();
        let b = rng.unit();
        let inner = encode(&RectParams { x, y, w, h, r, g, b, a: 1.0 })?;
        let outer = SceneNodeSetPayload {
            node_id: i,
            op_id:   scene_ops::ATRIUM_CORE_RECT,
            params:  inner,
        };
        wire += send_ctrl(conn, control::OP_SCENE_NODE_SET, &outer)?;
    }
    wire += send_ctrl(conn, control::OP_SCENE_FRAME_END, &SceneFrameEndPayload::default())?;
    log::info!("scene-a wire bytes: {} ({:.1} B/rect avg)",
        wire, wire as f32 / N as f32);
    Ok(())
}

/// Scene B: one 4 MB texture × 100 instances. Validates CAS dedup
/// (one upload, 100 references via slot_id).
fn scene_b(conn: &mut Connection) -> anyhow::Result<()> {
    const TEX_W: u32 = 1024;
    const TEX_H: u32 = 1024;
    const N: u32 = 100;
    log::info!("scene-b: {} byte texture × {N} instances",
        (TEX_W * TEX_H * 4) as u64);

    /* Build the texture: a deterministic checkerboard so we can
     * pixel-diff in step 12. */
    let mut tex = vec![0u8; (TEX_W * TEX_H * 4) as usize];
    for y in 0..TEX_H {
        for x in 0..TEX_W {
            let i = ((y * TEX_W + x) * 4) as usize;
            let on = ((x / 64) ^ (y / 64)) & 1 == 1;
            tex[i + 0] = if on { 0xFF } else { 0x20 };
            tex[i + 1] = if on { 0x80 } else { 0x60 };
            tex[i + 2] = if on { 0xC0 } else { 0xA0 };
            tex[i + 3] = 0xFF;
        }
    }

    /* CAS upload: travels the wire ONCE regardless of how many
     * instances reference it. atrium-rpc handles framing
     * (UPLOAD_BEGIN / DATA / FINISH) and returns the SHA-256. */
    log::info!("uploading texture (CAS)…");
    let hash = conn.upload_blob(&tex).context("upload_blob")?;
    log::info!("texture hash {}…", hex8(&hash));
    let mut wire = expected_upload_wire_bytes(tex.len());

    /* Bind hash to a slot the scene op can reference. The kind
     * descriptor tells the host shim how to interpret the bytes
     * — required because a 4 MB blob alone is ambiguous (could be
     * 1024×1024 RGBA, 2048×512, etc.). */
    let slot_id: u32 = 1;
    wire += send_ctrl(conn, control::OP_SLOT_SET, &SlotSetPayload {
        slot_id, hash,
        kind: SlotKind::Texture(TextureDesc {
            width:  TEX_W,
            height: TEX_H,
            format: TextureFormat::Rgba8UnormSrgb,
        }),
    })?;

    /* Compose the frame: N instances of the same texture, laid out
     * in a 10×10 grid. */
    wire += send_ctrl(conn, control::OP_SCENE_FRAME_BEGIN, &SceneFrameBeginPayload::default())?;
    let cell_w = 1920.0 / 10.0;
    let cell_h = 1080.0 / 10.0;
    let inset  = 4.0;
    for i in 0..N {
        let col = i % 10;
        let row = i / 10;
        let inner = encode(&TextureParams {
            x: col as f32 * cell_w + inset,
            y: row as f32 * cell_h + inset,
            w: cell_w - 2.0 * inset,
            h: cell_h - 2.0 * inset,
            slot_id,
        })?;
        let outer = SceneNodeSetPayload {
            node_id: i,
            op_id:   scene_ops::ATRIUM_CORE_TEXTURE,
            params:  inner,
        };
        wire += send_ctrl(conn, control::OP_SCENE_NODE_SET, &outer)?;
    }
    wire += send_ctrl(conn, control::OP_SCENE_FRAME_END, &SceneFrameEndPayload::default())?;

    /* ── Bandwidth assertion (step 11). ──
     *
     * Naive baseline: every instance carries its texture inline =
     * N * tex_size. Validates the §3.7 CAS-dedup thesis: large
     * resources travel the wire ONCE, scene nodes reference by 4-byte
     * slot_id. Expected ratio for scene-b: ~95–100×. */
    let tex_size  = (TEX_W * TEX_H * 4) as usize;
    let naive     = N as usize * tex_size;
    let ratio     = naive as f64 / wire as f64;
    log::info!(
        "scene-b wire bytes: {} ({:.2} MiB); naive: {} ({:.0} MiB); ratio: {:.1}×",
        wire, wire as f64 / 1048576.0,
        naive, naive as f64 / 1048576.0, ratio,
    );
    if ratio < 50.0 {
        anyhow::bail!("CAS dedup expected ≥ 50×, got {:.1}×", ratio);
    }
    Ok(())
}

/// Encode + send a control op payload. Flags = 0 for now (no
/// response expected, no async event, etc.). Returns the number of
/// wire bytes (envelope + payload) for the bandwidth accountant.
fn send_ctrl<T: serde::Serialize>(
    conn: &mut Connection,
    op: u16,
    payload: &T,
) -> anyhow::Result<usize> {
    let bytes = encode(payload)?;
    conn.send_message(CLASS_DISPLAY, op, 0, &bytes)?;
    Ok(ENVELOPE_BYTES + bytes.len())
}

// ── wire accountant ─────────────────────────────────────────────────
//
// atrium-rpc is a black box from outside the crate, so step 11's
// bandwidth assertion mirrors the documented framing rather than
// instrumenting Connection itself (which would touch a shared crate
// used by atrium-compositor + others).
//
// Constants kept in sync with atrium-rpc/src/envelope.rs and cas.rs:
const ENVELOPE_BYTES:    usize = 10;       /* HEADER_LEN */
const UPLOAD_INLINE_CAP: usize = 4096;     /* MAX_INLINE_BEGIN */
const UPLOAD_DATA_CHUNK: usize = 64 * 1024;/* upload_blob chunk size */
const HASH_BYTES:        usize = 32;       /* SHA-256 */

/// Mirror `Connection::upload_blob`'s framing to compute total wire
/// bytes for a blob of `len` bytes. The estimate is exact for the
/// payload portion; per-frame postcard varint headers (offset, len)
/// are upper-bounded at 16 B per UPLOAD_DATA frame.
fn expected_upload_wire_bytes(len: usize) -> usize {
    let inline = len.min(UPLOAD_INLINE_CAP);
    /* UPLOAD_BEGIN: env + hash + u64 total_size (~9B varint) + inline */
    let mut total = ENVELOPE_BYTES + HASH_BYTES + 9 + inline;
    let remaining = len - inline;
    let chunks = (remaining + UPLOAD_DATA_CHUNK - 1) / UPLOAD_DATA_CHUNK;
    /* Each UPLOAD_DATA: env + hash + u64 offset (~9B varint) + chunk */
    total += chunks * (ENVELOPE_BYTES + HASH_BYTES + 9);
    total += remaining;  /* the actual chunk bytes */
    if len > UPLOAD_INLINE_CAP {
        /* UPLOAD_FINISH: env + hash */
        total += ENVELOPE_BYTES + HASH_BYTES;
    }
    total
}

fn hex8(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{:02x}", b)).collect()
}

// ── small deterministic RNG ─────────────────────────────────────────
//
// xorshift64*; same seed → same scene. Used for Scene A so the test
// is reproducible and the pixel-diff step (step 12) can compare
// against a tiny-skia rendering of the same RNG sequence.

struct SmallRng { state: u64 }
impl SmallRng {
    fn new(seed: u64) -> Self { Self { state: if seed == 0 { 1 } else { seed } } }
    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(2685821657736338717)
    }
    fn unit(&mut self) -> f32 {
        (self.next() & 0xFFFF_FF) as f32 / 0x100_0000 as f32
    }
}
