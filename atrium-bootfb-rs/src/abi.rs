//! FFI types + ioctl numbers mirroring `atrium-kmod/bootfb/atrium_bootfb.h`.

pub const ATRIUM_BOOTFB_FORMAT_UNKNOWN: u32 = 0;
pub const ATRIUM_BOOTFB_FORMAT_BGRA8:   u32 = 1;
pub const ATRIUM_BOOTFB_FORMAT_RGBA8:   u32 = 2;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AtriumBootfbInfo {
    pub size:          u64,
    pub width:         u32,
    pub height:        u32,
    pub stride:        u32,
    pub format:        u32,
    pub mask_red:      u32,
    pub mask_green:    u32,
    pub mask_blue:     u32,
    pub mask_reserved: u32,
    pub bpp:           u32,
    pub reserved:      [u32; 7],
}

// `_IOR('A', 0x40, struct atrium_bootfb_info)` per FreeBSD's
// `_IOC(IOC_OUT, g, n, len)` = 0x40000000 | (len << 16) | (g << 8) | n.
// sizeof(AtriumBootfbInfo) = 72 bytes (8 + 4*9 + 4*7, all 8-aligned).
//   IOC_OUT   = 0x40000000
//   len(72)   = 0x00480000
//   'A' << 8  = 0x00004100
//   0x40      = 0x00000040
// = 0x40484140
pub const ATRIUM_BOOTFB_IOC_GET_INFO: libc::c_ulong = 0x40484140;
