//! atrium-textured — textured quad on the envelope-based wire.
//!
//! Migrated to `fresco-client` (M2.7c). Same diagonal-gradient +
//! checkerboard test pattern as the legacy version; on the wire,
//! the legacy 9-blob CAS tree collapses to upload_blob (texture
//! bytes) → slot_set_texture(slot=1) → scene_node_texture, since
//! atrium-core's TEXTURE op carries position+size+slot directly.

use fresco_client::Connection;
use fresco_protocol::{TextureFormat, TextureParams};

const TEX_W: u32 = 256;
const TEX_H: u32 = 256;

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    /* Build a 256×256 RGBA8 test pattern — diagonal gradient +
     * checkerboard, busy enough to expose sampling / orientation bugs
     * in a single screenshot. */
    let mut rgba = vec![0u8; (TEX_W * TEX_H * 4) as usize];
    for y in 0..TEX_H {
        for x in 0..TEX_W {
            let i = ((y * TEX_W + x) * 4) as usize;
            let cell = ((x / 32) ^ (y / 32)) & 1;
            let b = if cell == 0 { 60u8 } else { 200u8 };
            let r = ((x as u32 * 255) / TEX_W).min(255) as u8;
            let g = ((y as u32 * 255) / TEX_H).min(255) as u8;
            rgba[i + 0] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 0xff;
        }
    }

    /* Upload bytes → bind to slot → reference from a texture node. */
    let hash = conn.upload_blob(&rgba)?;
    eprintln!("uploaded texture {TEX_W}×{TEX_H}: {:02x}{:02x}..",
        hash[0], hash[1]);

    let slot_id = 1;
    conn.slot_set_texture(slot_id, hash, TEX_W, TEX_H,
        TextureFormat::Rgba8UnormSrgb)?;

    conn.scene_frame_begin()?;
    conn.scene_node_texture(/*node_id=*/ 0, TextureParams {
        x: 200.0, y: 200.0, w: 600.0, h: 600.0,
        slot_id,
    })?;
    conn.scene_frame_end()?;

    eprintln!("textured rect committed; ^C to exit");
    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
