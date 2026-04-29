//! "Hello, Fresco" rendered as native FreeBSD text on the host.
//!
//! Pipeline (no GTK/Pango/cairo/freetype):
//!   1. Load a TTF font from the FreeBSD filesystem
//!   2. Shape the string with rustybuzz (HarfBuzz-in-Rust)
//!   3. Rasterize each glyph with swash → packed alpha atlas
//!   4. Upload atlas as RGBA texture (NODE_TEXTURE)
//!   5. Build one quad per glyph → single mesh, single textured material
//!   6. Display via the slot graph
//!
//! Validates step (ii) of the long road. Edit `FONT_PATH` to point
//! at any TTF on the VM.

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection};

// Bundled DejaVu Sans in our test-assets so we don't depend on a
// pkg-installed font. Resolves under the 9p share at runtime.
const FONT_PATH: &str = "/mnt/host/test-assets/DejaVuSans.ttf";
const TEXT: &str       = "Hello, Fresco";
const FONT_SIZE_PX: f32 = 64.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let font_bytes = std::fs::read(FONT_PATH)
        .map_err(|e| format!("read {FONT_PATH}: {e} (try `pkg install dejavu`)"))?;

    let atlas = fresco_text::shape_and_rasterize(&font_bytes, TEXT, FONT_SIZE_PX)?;
    println!("shaped \"{TEXT}\" @ {FONT_SIZE_PX}px → {} glyphs, {:.1}px wide",
        atlas.glyphs.len(), atlas.advance);

    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    // Upload atlas as a texture.
    let tex_h = conn.cas_put_texture(atlas.width, atlas.height, &atlas.pixels)?;

    // Convert pixel-space glyph quads to a scene-units mesh. Pick a
    // scale that puts the text comfortably inside [-0.5..0.5] in NDC:
    // we have ~512 atlas units, want full text run ~0.6 wide.
    let pixel_to_unit = 0.6 / atlas.advance.max(1.0);
    let origin_x = -atlas.advance * pixel_to_unit * 0.5;     // center horizontally
    let origin_y = -atlas.ascent  * pixel_to_unit * 0.5 + atlas.ascent * pixel_to_unit;

    let (verts, indices) = fresco_text::build_text_mesh(&atlas, pixel_to_unit, origin_x, origin_y);

    let vert_h = conn.cas_put(&blob::vertex_data(&verts))?;
    let idx_h  = conn.cas_put(&blob::index_data(&indices))?;
    let mesh_h = conn.cas_put(&blob::mesh(
        (verts.len() / 5) as u32,
        indices.len() as u32,
        0x0500,                                  // POSITION + UV
        &vert_h, &idx_h,
    ))?;

    // White-tinted textured material (atlas already has R=G=B=255,
    // A=coverage; tint multiplies, so 0xffffffff = white text).
    let mat_h  = conn.cas_put(&blob::material_textured(&tex_h, 0xffffffff))?;
    let rend_h = conn.cas_put(&blob::renderable(&mesh_h, &mat_h))?;

    let cam_xform = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 5.0, 1.0,
    ];
    let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
    let aspect = disp.width as f32 / disp.height as f32;
    let cam_h = conn.cas_put(&blob::camera(0.7853982, aspect, 0.1, 100.0, &cam_xform_h))?;
    conn.set_camera(&cam_h)?;

    let slot = 1u16;
    conn.slot_alloc(slot, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(slot, &matrix_identity())?;
    conn.slot_set_content(slot, &rend_h)?;
    conn.slot_set_root(slot)?;

    conn.frame_begin(0)?;
    conn.frame_end()?;

    println!("rendered — sleeping 10 s. Look at the Fresco window.");
    std::thread::sleep(std::time::Duration::from_secs(10));
    Ok(())
}
