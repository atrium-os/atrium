//! Raw FFI bindings to libfresco.
//!
//! Hand-written rather than `bindgen`'d — the surface is small enough
//! and avoids pulling in libclang at build time. Mirrors fresco.h
//! exactly; if a function is added there, mirror it here.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use std::os::raw::{c_char, c_float, c_int, c_void};

pub type fresco_t = c_void;
pub type fresco_hash_t = [u8; 32];

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct fresco_display_t {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct fresco_completion_t {
    pub comp_type: u16,
    pub status: u16,
    pub id: u32,
    pub result_hash: [u8; 32],
    pub _pad: [u8; 88],   // [T; N>32] doesn't implement Default; that's OK
}                          // — we never need a default-initialized completion.

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct fresco_input_t {
    pub event_type: u16,
    pub code: u16,
    pub value_a: i32,
    pub value_b: i32,
    pub target_window: u32,
}

// Slot type and visible flag
pub type fresco_slot_t = u16;
pub const FRESCO_SLOT_FLAG_VISIBLE: u32 = 0x01;
pub const FRESCO_SLOT_FLAG_CLIP: u32 = 0x08;

// Blob type IDs (subset we use)
pub const FRESCO_NODE_RENDERABLE: u16 = 0x0005;
pub const FRESCO_NODE_TRANSFORM: u16 = 0x0004;
pub const FRESCO_NODE_CAMERA: u16 = 0x0003;
pub const FRESCO_NODE_MATERIAL_SOLID: u16 = 0x0200;
pub const FRESCO_NODE_MESH: u16 = 0x0100;
pub const FRESCO_NODE_VERTEX_DATA: u16 = 0x0110;
pub const FRESCO_NODE_INDEX_DATA: u16 = 0x0111;

// Input event types
pub const FRESCO_INPUT_KEY: u16 = 1;
pub const FRESCO_INPUT_MOUSE_MOVE: u16 = 2;
pub const FRESCO_INPUT_MOUSE_BUTTON: u16 = 3;
pub const FRESCO_INPUT_SCROLL: u16 = 4;
pub const FRESCO_INPUT_RESIZE: u16 = 5;

// Completion types
pub const FRESCO_COMP_QUERY_RESULT: u16 = 0x03;
pub const FRESCO_STATUS_NOT_FOUND: u16 = 0x04;
pub const FRESCO_STATUS_EXISTS: u16 = 0x03;

// Async window-event kinds (mirrors libfresco/include/fresco.h)
pub const FRESCO_WIN_EVENT_RESIZED: u16          = 0x11;
pub const FRESCO_WIN_EVENT_CLOSE_REQUESTED: u16  = 0x12;
pub const FRESCO_WIN_EVENT_FOCUS: u16            = 0x13;

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct fresco_window_event_t {
    pub kind: u16,
    pub _pad0: u16,
    pub window_id: u32,
    pub value_a: i32,
    pub value_b: i32,
}

extern "C" {
    pub fn fresco_open(dev_path: *const c_char) -> *mut fresco_t;
    pub fn fresco_close(f: *mut fresco_t);
    pub fn fresco_fd(f: *const fresco_t) -> c_int;

    pub fn fresco_get_display(f: *mut fresco_t, out: *mut fresco_display_t) -> c_int;
    pub fn fresco_get_system_font(f: *mut fresco_t, out: *mut u8) -> c_int;
    pub fn fresco_wait(f: *mut fresco_t, ms: c_int) -> c_int;

    // CAS
    pub fn fresco_cas_put(f: *mut fresco_t, data: *const c_void, len: usize, out: *mut u8) -> c_int;
    pub fn fresco_cas_query(f: *mut fresco_t, hash: *const u8) -> c_int;

    // Blob builders (write into caller buffer, return blob length)
    pub fn fresco_blob_material_solid(out: *mut u8, r: c_float, g: c_float, b: c_float, a: c_float) -> usize;
    pub fn fresco_blob_vertex_data(out: *mut u8, verts: *const c_float, n: usize) -> usize;
    pub fn fresco_blob_index_data(out: *mut u8, idx: *const u16, n: usize) -> usize;
    pub fn fresco_blob_mesh(
        out: *mut u8,
        vertex_count: u32,
        index_count: u32,
        flags: u32,
        vert_hash: *const u8,
        idx_hash: *const u8,
    ) -> usize;
    pub fn fresco_blob_renderable(out: *mut u8, mesh: *const u8, mat: *const u8) -> usize;
    pub fn fresco_blob_transform(out: *mut u8, m: *const c_float) -> usize;
    pub fn fresco_blob_camera(
        out: *mut u8,
        fov_y: c_float, aspect: c_float, near: c_float, far: c_float,
        view_xform: *const u8,
    ) -> usize;
    pub fn fresco_blob_pixel_data(out: *mut u8, rgba8: *const c_void, len: usize) -> usize;
    pub fn fresco_blob_texture(
        out: *mut u8,
        width: u32, height: u32,
        format: u8, filter: u8, wrap: u8,
        pixel_data_hash: *const u8,
    ) -> usize;
    pub fn fresco_blob_material_textured(
        out: *mut u8,
        texture_hash: *const u8,
        tint_rgba: u32,
    ) -> usize;
    pub fn fresco_cas_put_texture(
        f: *mut fresco_t,
        width: u32, height: u32,
        rgba8: *const c_void, bytes: usize,
        out: *mut u8,
    ) -> c_int;

    // Slot graph
    pub fn fresco_slot_alloc(f: *mut fresco_t, slot_id: fresco_slot_t, node_type: u16, flags: u32) -> c_int;
    pub fn fresco_slot_free(f: *mut fresco_t, slot_id: fresco_slot_t) -> c_int;
    pub fn fresco_slot_set_xform_inline(f: *mut fresco_t, slot_id: fresco_slot_t, m: *const c_float) -> c_int;
    pub fn fresco_slot_set_content(f: *mut fresco_t, slot_id: fresco_slot_t, content: *const u8) -> c_int;
    pub fn fresco_slot_set_root(f: *mut fresco_t, slot_id: fresco_slot_t) -> c_int;
    pub fn fresco_slot_set_children(
        f: *mut fresco_t,
        slot_id: fresco_slot_t,
        children: *const fresco_slot_t,
        n: usize,
    ) -> c_int;

    // Multi-window lifecycle (phase B1)
    pub fn fresco_create_window(
        f: *mut fresco_t,
        width: u32, height: u32, flags: u32,
        title: *const c_char,
        out: *mut u16,
    ) -> c_int;
    pub fn fresco_destroy_window(f: *mut fresco_t, window_id: u16) -> c_int;
    pub fn fresco_window_set_title(
        f: *mut fresco_t,
        window_id: u16,
        title: *const c_char,
    ) -> c_int;
    pub fn fresco_set_default_window(f: *mut fresco_t, window_id: u16);
    pub fn fresco_window_set_pos(f: *mut fresco_t, window_id: u16,
                                 x: c_float, y: c_float) -> c_int;
    pub fn fresco_window_set_size(f: *mut fresco_t, window_id: u16,
                                  width: u32, height: u32) -> c_int;
    pub fn fresco_client_slot(f: *const fresco_t) -> u32;
    pub fn fresco_set_camera(f: *mut fresco_t, camera_hash: *const u8) -> c_int;
    pub fn fresco_frame_begin(f: *mut fresco_t, frame_number: u32) -> c_int;
    pub fn fresco_frame_end(f: *mut fresco_t) -> c_int;
    pub fn fresco_matrix_identity(out: *mut c_float);

    // Input
    pub fn fresco_input_poll(f: *mut fresco_t, out: *mut fresco_input_t) -> c_int;
    pub fn fresco_input_wait(f: *mut fresco_t, out: *mut fresco_input_t, ms: c_int) -> c_int;

    pub fn fresco_window_event_poll(f: *mut fresco_t, out: *mut fresco_window_event_t) -> c_int;
    pub fn fresco_window_event_wait(f: *mut fresco_t, out: *mut fresco_window_event_t, ms: c_int) -> c_int;

    pub fn fresco_event_wait(
        f: *mut fresco_t,
        in_out: *mut fresco_input_t,
        window_out: *mut fresco_window_event_t,
        ms: c_int,
    ) -> c_int;

    // Raw escape hatch
    pub fn fresco_raw_submit(
        f: *mut fresco_t,
        opcode: u16, flags: u16, sequence_id: u32,
        payload: *const c_void, payload_len: usize,
    ) -> c_int;
}
