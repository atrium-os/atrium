//! Pre-encoded virtio-gpu protocol bytes for use as IOC_SUBMIT command
//! buffers. The kernel pushes these onto the controlq verbatim.
//!
//! v0.1 only encodes the minimum needed for D1 backend bring-up:
//! `RESOURCE_CREATE_2D`, `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`. The
//! display path's `SET_MODE` / `PAGE_FLIP` ioctls already issue
//! `CREATE_2D` + `ATTACH_BACKING` + `SET_SCANOUT` + `TRANSFER` + `FLUSH`
//! internally, so a userspace server typically does not call these
//! directly — they're here for completeness and for the future
//! "submit a vendor-specific command stream" path.

#![allow(dead_code)]

pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D:    u32 = 0x0101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF:        u32 = 0x0102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT:           u32 = 0x0103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH:        u32 = 0x0104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D:   u32 = 0x0105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING:   u32 = 0x0106;
pub const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING:   u32 = 0x0107;

pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;

pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;

#[repr(C, packed)]
pub struct CtrlHdr {
    pub r#type: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

#[repr(C, packed)]
pub struct ResourceCreate2d {
    pub hdr: CtrlHdr,
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C, packed)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C, packed)]
pub struct TransferToHost2d {
    pub hdr: CtrlHdr,
    pub r: Rect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}

#[repr(C, packed)]
pub struct ResourceFlush {
    pub hdr: CtrlHdr,
    pub r: Rect,
    pub resource_id: u32,
    pub padding: u32,
}

/// Build a header ready to push onto the controlq.
pub fn hdr(cmd: u32) -> CtrlHdr {
    CtrlHdr {
        r#type: cmd.to_le(),
        flags: VIRTIO_GPU_FLAG_FENCE.to_le(),
        fence_id: 0,
        ctx_id: 0,
        ring_idx: 0,
        padding: [0; 3],
    }
}
