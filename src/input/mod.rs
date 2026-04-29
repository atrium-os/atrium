//! Input subsystem.
//!
//! `InputEvent` is the cross-platform wire-format struct that all
//! platform-specific capture impls produce. It's defined here (not in
//! a capture submodule) so non-input modules — ivshmem, network — can
//! reference the type without depending on a winit-coupled adapter.
//!
//! `capture` (winit-based, macOS host) and future `freebsd_hid`
//! produce `InputEvent` values; consumers stay platform-neutral.

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value_a: i32,
    pub value_b: i32,
    /// Server-determined target window. For pointer events, this is
    /// the window under the cursor; for keys, the focused window;
    /// 0 = screen / no specific window. Apps filter input by this.
    pub target_window: u32,
    pub _pad0: [u32; 12],
}

pub const INPUT_KEY: u16 = 1;
pub const INPUT_MOUSE_MOVE: u16 = 2;
pub const INPUT_MOUSE_BUTTON: u16 = 3;
pub const INPUT_SCROLL: u16 = 4;
pub const INPUT_RESIZE: u16 = 5;

#[cfg(target_os = "macos")]
pub mod capture;
