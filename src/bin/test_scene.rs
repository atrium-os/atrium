use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use memmap2::MmapMut;
use sha2::{Sha256, Digest};
use ttf_parser;

const CTRL_OFFSET: usize = 0x0000;
const CTRL_CMD_READ: usize = 4;
const CMD_RING_OFFSET: usize = 0x1000;
const ENTRY_SIZE: usize = 128;
const RING_ENTRIES: usize = 256;

fn main() {
    let shmem_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/karythra-gpu-shmem"));

    println!("test_scene: opening {:?}", shmem_path);
    println!("start the gpu server first: cargo run -- {:?}", shmem_path);
    println!();

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&shmem_path)
        .expect("failed to open shmem - is the gpu server running?");

    let mut mmap = unsafe { MmapMut::map_mut(&file).unwrap() };

    let status = read_u32(&mmap, CTRL_OFFSET + 24);
    let display_w = read_u32(&mmap, CTRL_OFFSET + 28);
    let display_h = read_u32(&mmap, CTRL_OFFSET + 32);
    println!("server status={} display={}x{}", status, display_w, display_h);

    // ── Build scene: vector 2D UI elements + 3D rotating triangle ─

    // identity transform
    let identity = make_transform(&[
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ]);
    let identity_hash = upload(&mut mmap, &identity);

    let mut nodes: Vec<[u8; 32]> = Vec::new();

    // helper: create a vector path scene node
    let make_path_node = |mmap: &mut MmapMut, segs: &[Vec<u8>], rgba: u32, identity_hash: &[u8; 32]| -> [u8; 32] {
        let path_data = make_path_data(segs);
        let seg_count = segs.len() as u32;
        let pd_hash = upload(mmap, &path_data);
        let ph = make_path_header(0x01, 0.0005, seg_count, &pd_hash); // FILL, fine tolerance
        let ph_hash = upload(mmap, &ph);
        let mat = make_material(rgba);
        let mat_hash = upload(mmap, &mat);
        let rend = make_renderable(&ph_hash, &mat_hash);
        let rend_hash = upload(mmap, &rend);
        let node = make_scene_node(0x01, identity_hash, &rend_hash);
        upload(mmap, &node)
    };

    // window background (dark gray, rounded rect)
    let win_bg = make_path_node(&mut mmap,
        &make_rounded_rect_path(-0.7, 0.7, 1.4, 1.2, 0.05),
        0xFF333333, &identity_hash);
    nodes.push(win_bg);

    // title bar (orange, rounded rect)
    let title_bar = make_path_node(&mut mmap,
        &make_rounded_rect_path(-0.7, 0.7, 1.4, 0.12, 0.03),
        0xFFCC6622, &identity_hash);
    nodes.push(title_bar);

    // button 1 (green, rounded rect)
    let btn1 = make_path_node(&mut mmap,
        &make_rounded_rect_path(-0.5, 0.4, 0.4, 0.15, 0.04),
        0xFF00AA44, &identity_hash);
    nodes.push(btn1);

    // button 2 (blue, rounded rect)
    let btn2 = make_path_node(&mut mmap,
        &make_rounded_rect_path(0.1, 0.4, 0.4, 0.15, 0.04),
        0xFF4444DD, &identity_hash);
    nodes.push(btn2);

    // circular decoration (yellow circle)
    let circle = make_path_node(&mut mmap,
        &make_circle_path(0.0, -0.15, 0.15),
        0xFF00CCFF, &identity_hash);
    nodes.push(circle);

    // content area (lighter gray, rounded rect)
    let content = make_path_node(&mut mmap,
        &make_rounded_rect_path(-0.6, 0.15, 1.2, 0.65, 0.03),
        0xFF444444, &identity_hash);
    nodes.push(content);

    // text rendered from font outlines
    let font_bytes = std::fs::read("/System/Library/Fonts/Supplemental/Arial Bold.ttf")
        .expect("failed to load font");
    let face = ttf_parser::Face::parse(&font_bytes, 0).expect("failed to parse font");
    let units_per_em = face.units_per_em() as f32;

    let text = "Fresco";
    let text_size = 0.08;
    let mut text_x: f32 = -0.45;
    let text_y: f32 = 0.62;
    let text_scale = text_size / units_per_em;

    for ch in text.chars() {
        if let Some(glyph_id) = face.glyph_index(ch) {
            let mut builder = GlyphOutlineBuilder::new(text_scale, text_x, text_y);
            if face.outline_glyph(glyph_id, &mut builder).is_some() && !builder.segments.is_empty() {
                let path_data = builder.to_path_data();
                let seg_count = builder.segments.len() as u32;
                let pd_hash = upload(&mut mmap, &path_data);
                let ph = make_path_header(0x01, 0.0005, seg_count, &pd_hash);
                let ph_hash = upload(&mut mmap, &ph);
                let mat = make_material(0xFFFFFFFF); // white text
                let mat_hash = upload(&mut mmap, &mat);
                let rend = make_renderable(&ph_hash, &mat_hash);
                let rend_hash = upload(&mut mmap, &rend);
                let node = make_scene_node(0x01, &identity_hash, &rend_hash);
                let node_hash = upload(&mut mmap, &node);
                nodes.push(node_hash);
            }
            if let Some(adv) = face.glyph_hor_advance(glyph_id) {
                text_x += adv as f32 * text_scale;
            }
        } else if ch == ' ' {
            text_x += text_size * 0.3;
        }
    }

    // 3D triangle (rendered in perspective, will rotate)
    let tri_verts: Vec<u8> = [
         0.0f32,  0.4, 0.0,
        -0.3,    -0.2, 0.0,
         0.3,    -0.2, 0.0,
    ].iter().flat_map(|f| f.to_le_bytes()).collect();
    let tri_vert_hash = upload(&mut mmap, &tri_verts);
    let tri_mesh = make_mesh(3, 12, &tri_vert_hash);
    let tri_mesh_hash = upload(&mut mmap, &tri_mesh);
    let tri_mat = make_material(0xFF00FFFF); // yellow
    let tri_mat_hash = upload(&mut mmap, &tri_mat);
    let tri_rend = make_renderable(&tri_mesh_hash, &tri_mat_hash);
    let tri_rend_hash = upload(&mut mmap, &tri_rend);

    // camera at z=3
    let cam_xform = make_transform(&[
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 3.0, 1.0,
    ]);
    let cam_xform_hash = upload(&mut mmap, &cam_xform);
    let camera = make_camera(
        std::f32::consts::FRAC_PI_4,
        display_w as f32 / display_h.max(1) as f32,
        0.1, 100.0,
        &cam_xform_hash,
    );
    let cam_hash = upload(&mut mmap, &camera);

    // initial scene with triangle at identity
    let tri_node = make_scene_node(0x01, &identity_hash, &tri_rend_hash);
    let tri_node_hash = upload(&mut mmap, &tri_node);
    nodes.push(tri_node_hash);

    let list = make_node_list(&nodes);
    let list_hash = upload(&mut mmap, &list);
    let root = make_scene_root(&list_hash, &cam_hash);
    let root_hash = upload(&mut mmap, &root);

    send_cmd(&mut mmap, 0x0100, &root_hash, &[0u8; 32]);
    send_cmd(&mut mmap, 0x0300, &[0u8; 32], &[0u8; 32]);

    println!("\nscene submitted — vector UI + rotating triangle");
    println!("ctrl-c to exit");

    let tri_idx = nodes.len() - 1; // triangle is the last node

    // animate: rotate the yellow triangle inside the "window"
    let mut angle: f32 = 0.0;
    loop {
        thread::sleep(Duration::from_millis(16));
        angle += 0.03;

        let cos = angle.cos();
        let sin = angle.sin();
        let rot_xform = make_transform(&[
             cos, sin, 0.0, 0.0,
            -sin, cos, 0.0, 0.0,
             0.0, 0.0, 1.0, 0.0,
             0.0, -0.15, 0.0, 1.0,
        ]);
        let rot_xform_hash = upload(&mut mmap, &rot_xform);
        let rot_tri = make_scene_node(0x01, &rot_xform_hash, &tri_rend_hash);
        let rot_tri_hash = upload(&mut mmap, &rot_tri);

        nodes[tri_idx] = rot_tri_hash;
        let new_list = make_node_list(&nodes);
        let new_list_hash = upload(&mut mmap, &new_list);
        let new_root = make_scene_root(&new_list_hash, &cam_hash);
        let new_root_hash = upload(&mut mmap, &new_root);

        send_cmd(&mut mmap, 0x0100, &new_root_hash, &[0u8; 32]);
        send_cmd(&mut mmap, 0x0300, &[0u8; 32], &[0u8; 32]);
    }
}

// ── Shared memory protocol ──────────────────────────────────────

fn read_u32(mmap: &MmapMut, offset: usize) -> u32 {
    u32::from_le_bytes([mmap[offset], mmap[offset+1], mmap[offset+2], mmap[offset+3]])
}

fn write_u32(mmap: &mut MmapMut, offset: usize, val: u32) {
    mmap[offset..offset+4].copy_from_slice(&val.to_le_bytes());
}

static mut SEQ: u32 = 0;
fn next_seq() -> u32 { unsafe { SEQ += 1; SEQ } }

fn wait_ring_space(mmap: &MmapMut) {
    loop {
        let write_ptr = read_u32(mmap, CTRL_OFFSET);
        let read_ptr = read_u32(mmap, CTRL_OFFSET + CTRL_CMD_READ);
        let pending = write_ptr.wrapping_sub(read_ptr) as usize;
        if pending < RING_ENTRIES - 4 { break; }
        thread::sleep(Duration::from_micros(100));
    }
}

fn send_cmd(mmap: &mut MmapMut, opcode: u16, hash_a: &[u8; 32], hash_b: &[u8; 32]) {
    wait_ring_space(mmap);
    let write_ptr = read_u32(mmap, CTRL_OFFSET);
    let index = (write_ptr as usize) % RING_ENTRIES;
    let base = CMD_RING_OFFSET + index * ENTRY_SIZE;
    let seq = next_seq();

    for i in 0..ENTRY_SIZE { mmap[base + i] = 0; }
    mmap[base..base+2].copy_from_slice(&opcode.to_le_bytes());
    mmap[base+4..base+8].copy_from_slice(&seq.to_le_bytes());
    mmap[base+8..base+40].copy_from_slice(hash_a);
    mmap[base+40..base+72].copy_from_slice(hash_b);

    write_u32(mmap, CTRL_OFFSET, write_ptr.wrapping_add(1));
}

fn write_raw_cmd(mmap: &mut MmapMut, opcode: u16, seq: u32, payload: &[u8]) {
    wait_ring_space(mmap);
    let write_ptr = read_u32(mmap, CTRL_OFFSET);
    let index = (write_ptr as usize) % RING_ENTRIES;
    let base = CMD_RING_OFFSET + index * ENTRY_SIZE;

    for i in 0..ENTRY_SIZE { mmap[base + i] = 0; }
    mmap[base..base+2].copy_from_slice(&opcode.to_le_bytes());
    mmap[base+4..base+8].copy_from_slice(&seq.to_le_bytes());
    let n = payload.len().min(120);
    mmap[base+8..base+8+n].copy_from_slice(&payload[..n]);
    write_u32(mmap, CTRL_OFFSET, write_ptr.wrapping_add(1));
}

fn upload(mmap: &mut MmapMut, data: &[u8]) -> [u8; 32] {
    let hash = sha256(data);
    let seq = next_seq();

    // CMD_UPLOAD_BEGIN: size(4) + granularity(1) + pad(3) + data(up to 112)
    let mut begin_payload = vec![0u8; 120];
    begin_payload[0..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
    begin_payload[4] = if data.len() <= 128 { 0 } else { 1 };
    let first_chunk = data.len().min(112);
    begin_payload[8..8+first_chunk].copy_from_slice(&data[..first_chunk]);
    write_raw_cmd(mmap, 0x0001, seq, &begin_payload);

    // continuation chunks
    let mut pos = first_chunk;
    while pos < data.len() {
        let chunk = (data.len() - pos).min(116);
        let mut data_payload = vec![0u8; 120];
        data_payload[0..4].copy_from_slice(&(pos as u32).to_le_bytes());
        data_payload[4..4+chunk].copy_from_slice(&data[pos..pos+chunk]);
        write_raw_cmd(mmap, 0x0002, seq, &data_payload);
        pos += chunk;
    }

    // CMD_UPLOAD_FINISH
    let mut finish_payload = vec![0u8; 120];
    finish_payload[32..36].copy_from_slice(&seq.to_le_bytes()); // upload_id
    write_raw_cmd(mmap, 0x0003, seq, &finish_payload);

    hash
}

// ── Node builders ───────────────────────────────────────────────

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let r = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&r);
    out
}

fn make_transform(m: &[f32; 16]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x03;
    b[4..8].copy_from_slice(&1.0f32.to_le_bytes());
    for i in 0..16 { b[64+i*4..68+i*4].copy_from_slice(&m[i].to_le_bytes()); }
    b
}

fn make_mesh(vcount: u32, vstride: u16, vdata: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x08;
    b[2..6].copy_from_slice(&vcount.to_le_bytes());
    b[10..12].copy_from_slice(&vstride.to_le_bytes());
    b[32..64].copy_from_slice(vdata);
    b
}

fn make_indexed_mesh(
    vcount: u32, icount: u32, vstride: u16, idx_fmt: u16,
    vdata: &[u8; 32], idata: &[u8; 32],
) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x08;
    b[1] = 0x01; // INDEXED flag
    b[2..6].copy_from_slice(&vcount.to_le_bytes());
    b[6..10].copy_from_slice(&icount.to_le_bytes());
    b[10..12].copy_from_slice(&vstride.to_le_bytes());
    b[12..14].copy_from_slice(&idx_fmt.to_le_bytes()); // 0=u16, 1=u32
    b[32..64].copy_from_slice(vdata);
    b[64..96].copy_from_slice(idata);
    b
}

fn make_quad_verts(x: f32, y: f32, w: f32, h: f32) -> Vec<u8> {
    // 4 vertices for a quad in NDC-like coordinates
    let verts: [f32; 12] = [
        x,     y,     0.5,  // top-left
        x + w, y,     0.5,  // top-right
        x + w, y - h, 0.5,  // bottom-right
        x,     y - h, 0.5,  // bottom-left
    ];
    verts.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn make_quad_indices() -> Vec<u8> {
    let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
    indices.iter().flat_map(|i| i.to_le_bytes()).collect()
}

fn make_material(rgba: u32) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x05;
    b[2..6].copy_from_slice(&rgba.to_le_bytes());
    b
}

fn make_renderable(mesh: &[u8; 32], mat: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x04;
    b[32..64].copy_from_slice(mesh);
    b[64..96].copy_from_slice(mat);
    b
}

fn make_scene_node(flags: u8, xform: &[u8; 32], rend: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x02;
    b[1] = flags;
    b[32..64].copy_from_slice(xform);
    b[64..96].copy_from_slice(rend);
    b
}

fn make_node_list(entries: &[[u8; 32]]) -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    b[0] = 0x10;
    b[1] = entries.len() as u8;
    for (i, h) in entries.iter().enumerate() {
        let off = 36 + i * 32;
        b[off..off+32].copy_from_slice(h);
    }
    b
}

fn make_camera(fov: f32, aspect: f32, near: f32, far: f32, xform: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x06;
    b[4..8].copy_from_slice(&fov.to_le_bytes());
    b[8..12].copy_from_slice(&aspect.to_le_bytes());
    b[12..16].copy_from_slice(&near.to_le_bytes());
    b[16..20].copy_from_slice(&far.to_le_bytes());
    b[32..64].copy_from_slice(xform);
    b
}

fn make_scene_root(list: &[u8; 32], cam: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x01;
    b[32..64].copy_from_slice(list);
    b[64..96].copy_from_slice(cam);
    b
}

// ── Vector path helpers ─────────────────────────────────────────

fn path_seg(seg_type: u8, points: &[f32]) -> Vec<u8> {
    let mut b = vec![0u8; 28];
    b[0] = seg_type;
    for (i, &f) in points.iter().enumerate() {
        b[4 + i * 4..8 + i * 4].copy_from_slice(&f.to_le_bytes());
    }
    b
}

fn make_path_data(segments: &[Vec<u8>]) -> Vec<u8> {
    let mut data = Vec::new();
    for seg in segments {
        data.extend_from_slice(seg);
    }
    data
}

fn make_path_header(flags: u8, tolerance: f32, seg_count: u32, path_data_hash: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 128];
    b[0] = 0x0D; // PATH_HEADER
    b[1] = flags;
    b[12..16].copy_from_slice(&tolerance.to_le_bytes());
    b[16..20].copy_from_slice(&seg_count.to_le_bytes());
    b[32..64].copy_from_slice(path_data_hash);
    b
}

fn make_rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Vec<Vec<u8>> {
    // approximate quarter-circle with cubic bezier: control point offset = r * 0.5522847498
    let k = r * 0.5522847498;
    let x1 = x;
    let x2 = x + w;
    let y1 = y;
    let y2 = y - h; // y goes down

    vec![
        path_seg(0, &[x1 + r, y1]),                                          // MoveTo top-left + r
        path_seg(1, &[x2 - r, y1]),                                          // LineTo top-right - r
        path_seg(3, &[x2 - r + k, y1, x2, y1 - k, x2, y1 - r]),            // CubicTo top-right corner
        path_seg(1, &[x2, y2 + r]),                                          // LineTo bottom-right + r
        path_seg(3, &[x2, y2 + r - k, x2 - r + k, y2, x2 - r, y2]),        // CubicTo bottom-right corner
        path_seg(1, &[x1 + r, y2]),                                          // LineTo bottom-left + r
        path_seg(3, &[x1 + r - k, y2, x1, y2 + r - k, x1, y2 + r]),        // CubicTo bottom-left corner
        path_seg(1, &[x1, y1 - r]),                                          // LineTo top-left + r (up)
        path_seg(3, &[x1, y1 - r + k, x1 + r - k, y1, x1 + r, y1]),        // CubicTo top-left corner
        path_seg(5, &[]),                                                    // Close
    ]
}

fn make_circle_path(cx: f32, cy: f32, r: f32) -> Vec<Vec<u8>> {
    let k = r * 0.5522847498;
    vec![
        path_seg(0, &[cx + r, cy]),
        path_seg(3, &[cx + r, cy + k, cx + k, cy + r, cx, cy + r]),
        path_seg(3, &[cx - k, cy + r, cx - r, cy + k, cx - r, cy]),
        path_seg(3, &[cx - r, cy - k, cx - k, cy - r, cx, cy - r]),
        path_seg(3, &[cx + k, cy - r, cx + r, cy - k, cx + r, cy]),
        path_seg(5, &[]),
    ]
}

// ── Font glyph outline builder ──────────────────────────────────

struct GlyphOutlineBuilder {
    segments: Vec<Vec<u8>>,
    scale: f32,
    ox: f32,
    oy: f32,
}

impl GlyphOutlineBuilder {
    fn new(scale: f32, ox: f32, oy: f32) -> Self {
        Self { segments: Vec::new(), scale, ox, oy }
    }
    fn tx(&self, x: f32) -> f32 { x * self.scale + self.ox }
    fn ty(&self, y: f32) -> f32 { y * self.scale + self.oy }
    fn to_path_data(&self) -> Vec<u8> {
        make_path_data(&self.segments)
    }
}

impl ttf_parser::OutlineBuilder for GlyphOutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.segments.push(path_seg(0, &[self.tx(x), self.ty(y)]));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.segments.push(path_seg(1, &[self.tx(x), self.ty(y)]));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.segments.push(path_seg(2, &[self.tx(x1), self.ty(y1), self.tx(x), self.ty(y)]));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.segments.push(path_seg(3, &[
            self.tx(x1), self.ty(y1), self.tx(x2), self.ty(y2), self.tx(x), self.ty(y),
        ]));
    }
    fn close(&mut self) {
        self.segments.push(path_seg(5, &[]));
    }
}
