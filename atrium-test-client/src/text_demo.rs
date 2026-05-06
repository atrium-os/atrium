//! atrium-text-demo — text on the FreeBSD-native stack via the
//! atrium-text bundle's `glyph_run` op (M6.1).
//!
//! Old shape: one CAS blob + one Texture slot + one TEXTURE node *per
//! glyph* (~14 slots / ~14 nodes for "Hello, FreeBSD!"). Wasteful but
//! proved the chain end-to-end.
//!
//! New shape: one R8 atlas blob + one Texture slot + one GLYPH_RUN
//! node carrying all 14 GlyphInstances. Atlas coverage is multiplied
//! against the run's tint colour in the bundle's fragment shader.

use fresco_client::Connection;
use fresco_protocol::TextureFormat;
use fresco_text::shape_and_rasterize;

const FONT_PATH:  &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const TEXT:       &str = "Hello, FreeBSD!";
const SIZE_PX:    f32  = 64.0;
const BASELINE_X: f32  = 80.0;
const BASELINE_Y: f32  = 400.0;

const ATLAS_SLOT: u32 = 100;
const RUN_NODE:   u32 = 200;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let font = std::fs::read(FONT_PATH)?;
    let atlas = shape_and_rasterize(&font, TEXT, SIZE_PX)?;
    if std::env::var("DUMP_ATLAS").is_ok() {
        use std::io::Write;
        let r8 = atlas.r8_pixels();
        let mut f = std::fs::File::create("/tmp/atlas_r8.raw")?;
        f.write_all(&r8)?;
        eprintln!("dumped atlas to /tmp/atlas_r8.raw ({}x{})",
                  atlas.width, atlas.height);
        return Ok(());
    }
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");
    eprintln!(
        "shaped {:?}: {} glyphs, atlas {}x{}, advance={:.1}",
        TEXT, atlas.glyphs.len(), atlas.width, atlas.height, atlas.advance,
    );

    // Upload the R8 coverage atlas as a single blob + slot.
    let r8 = atlas.r8_pixels();
    let hash = conn.upload_blob(&r8)?;
    conn.slot_set_texture(
        ATLAS_SLOT, hash,
        atlas.width, atlas.height,
        TextureFormat::R8Unorm,
    )?;

    // One glyph_run node carrying all glyphs, white tint.
    let params = atlas.to_glyph_run(
        BASELINE_X, BASELINE_Y,
        [1.0, 1.0, 1.0, 1.0],
        ATLAS_SLOT,
    );

    conn.scene_frame_begin()?;
    conn.scene_node_glyph_run(RUN_NODE, params)?;
    conn.scene_frame_end()?;
    eprintln!("emitted 1 glyph_run node with {} glyphs", atlas.glyphs.len());

    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
