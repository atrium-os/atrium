use crate::command::protocol::{Command, Completion};
use crate::input::InputEvent;

use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::Path;

// ── Memory layout ──────────────────────────────────────────────────
//
// 0x000000  ctrl regs         (4 KiB)
// 0x010000  per-client slots  (NUM_CLIENT_SLOTS × SLOT_STRIDE)
//             slot[i].cmd_ring   at SLOTS_BASE + i*SLOT_STRIDE
//             slot[i].comp_ring  at SLOTS_BASE + i*SLOT_STRIDE + 0x8000
//             slot[i].input_ring at SLOTS_BASE + i*SLOT_STRIDE + 0x10000
// SLOTS_BASE + N*SLOT_STRIDE  CAS staging
//
// Per-slot R/W pointers live in ctrl regs at PER_SLOT_REGS_OFFSET +
// 24*i (cmd_w, cmd_r, comp_w, comp_r, input_w, input_r).
//
// All three rings are per-slot. Cmd/comp follow the obvious pattern;
// input events are routed by the server to the owning client's slot
// based on `target_window` ownership (or focus for keyboard
// events). This gives clean isolation — a client only sees events
// for its own windows — and bounds the blast radius of a slow client.

pub const NUM_CLIENT_SLOTS: usize = 4;
const SLOT_STRIDE:  usize = 0x14000;     // 80 KiB per slot (cmd+comp+input rings)
const SLOTS_BASE:   usize = 0x10000;
const SLOT_CMD_OFFSET:   usize = 0x00000;
const SLOT_COMP_OFFSET:  usize = 0x08000;
const SLOT_INPUT_OFFSET: usize = 0x10000;
const STAGING_BASE:      usize = SLOTS_BASE + NUM_CLIENT_SLOTS * SLOT_STRIDE;
/// Per-slot staging region for CMD_UPLOAD_DMA. 7 MiB per slot — fits
/// a 4 MiB atlas with room for a second large blob queued. 4 slots
/// × 7 MiB = 28 MiB; total shmem usage = STAGING_BASE + 28 MiB.
pub const SLOT_STAGING_SIZE: usize = 0x700000;
const STAGING_OFFSET: usize = STAGING_BASE + NUM_CLIENT_SLOTS * SLOT_STAGING_SIZE;

const CMD_RING_ENTRIES: usize = 256;
const CMD_ENTRY_SIZE:   usize = 128;
const INPUT_RING_ENTRIES: usize = 256;   // 256 × 64 B = 16 KiB per slot
const INPUT_ENTRY_SIZE:   usize = 64;

// Control register layout
//
// Globals (server-only metadata):
//   [0x18]   status
//   [0x1c]   display_width
//   [0x20]   display_height
//   [0x24]   refresh_hz
//   [0x28]   system_font_hash (32 bytes)
//   [0x48]   current_window (legacy; unused with explicit window routing)
//
// Per-client (24 bytes per slot, starting at 0x100):
//   [0x100 + 24*i + 0]  cmd_write    (guest writes, server reads)
//   [0x100 + 24*i + 4]  cmd_read     (server writes)
//   [0x100 + 24*i + 8]  comp_write   (server writes)
//   [0x100 + 24*i +12]  comp_read    (guest writes)
//   [0x100 + 24*i +16]  input_write  (server writes)
//   [0x100 + 24*i +20]  input_read   (guest writes)

const CTRL_STATUS:        usize = 0x18;
const CTRL_DISPLAY_W:     usize = 0x1c;
const CTRL_DISPLAY_H:     usize = 0x20;
const CTRL_REFRESH_HZ:    usize = 0x24;
const CTRL_SYSTEM_FONT:   usize = 0x28; // 32 bytes
#[allow(dead_code)]
const CTRL_CURRENT_WINDOW:usize = 0x48;
/// Bit i set ⇒ slot i is held by a live cdev open(). Kmod sets the
/// bit on open, clears it in the cdevpriv destructor (which runs on
/// every fd close, including process death). Server polls this each
/// frame; any 1→0 transition triggers cleanup of windows owned by
/// the disconnected slot.
const CTRL_SLOTS_ALIVE_MASK: usize = 0x40;

const CTRL_PER_SLOT_BASE: usize = 0x100;
/// Per-slot ctrl reg stride. Currently 24 bytes are used (3 × ring
/// R/W u32 pairs); the extra 8 bytes are reserved for future state
/// that's cheap to grow into without moving every offset (e.g.
/// per-slot focus/visibility flags, mouse cursor pos override).
const CTRL_PER_SLOT_STRIDE: usize = 32;

#[inline]
fn ctrl_cmd_write(slot: usize)   -> usize { CTRL_PER_SLOT_BASE + CTRL_PER_SLOT_STRIDE * slot + 0 }
#[inline]
fn ctrl_cmd_read(slot: usize)    -> usize { CTRL_PER_SLOT_BASE + CTRL_PER_SLOT_STRIDE * slot + 4 }
#[inline]
fn ctrl_comp_write(slot: usize)  -> usize { CTRL_PER_SLOT_BASE + CTRL_PER_SLOT_STRIDE * slot + 8 }
#[inline]
fn ctrl_comp_read(slot: usize)   -> usize { CTRL_PER_SLOT_BASE + CTRL_PER_SLOT_STRIDE * slot + 12 }
#[inline]
fn ctrl_input_write(slot: usize) -> usize { CTRL_PER_SLOT_BASE + CTRL_PER_SLOT_STRIDE * slot + 16 }
#[inline]
fn ctrl_input_read(slot: usize)  -> usize { CTRL_PER_SLOT_BASE + CTRL_PER_SLOT_STRIDE * slot + 20 }

#[inline]
fn slot_cmd_offset(slot: usize)   -> usize { SLOTS_BASE + SLOT_STRIDE * slot + SLOT_CMD_OFFSET }
#[inline]
fn slot_comp_offset(slot: usize)  -> usize { SLOTS_BASE + SLOT_STRIDE * slot + SLOT_COMP_OFFSET }
#[inline]
fn slot_input_offset(slot: usize) -> usize { SLOTS_BASE + SLOT_STRIDE * slot + SLOT_INPUT_OFFSET }
#[inline]
pub fn slot_staging_offset(slot: usize) -> usize { STAGING_BASE + SLOT_STAGING_SIZE * slot }

pub struct IvshmemLink {
    mmap: MmapMut,
    size: usize,
}

impl IvshmemLink {
    pub fn open(path: &Path, size: usize) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        file.set_len(size as u64)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        let mut link = Self { mmap, size };

        // zero ctrl + input + slot regions on startup. CAS staging is
        // multi-MiB and lazily faulted; clearing it is wasteful and
        // unnecessary (CAS is content-addressed).
        for i in 0..STAGING_OFFSET.min(size) {
            link.mmap[i] = 0;
        }
        link.write_ctrl_u32(CTRL_STATUS, 1); // READY

        Ok(link)
    }

    pub fn status(&self) -> u32 {
        self.read_ctrl_u32(CTRL_STATUS)
    }

    pub fn set_status(&mut self, val: u32) {
        self.write_ctrl_u32(CTRL_STATUS, val);
    }

    pub fn reset_rings(&mut self) {
        for s in 0..NUM_CLIENT_SLOTS {
            self.write_ctrl_u32(ctrl_cmd_write(s),   0);
            self.write_ctrl_u32(ctrl_cmd_read(s),    0);
            self.write_ctrl_u32(ctrl_comp_write(s),  0);
            self.write_ctrl_u32(ctrl_comp_read(s),   0);
            self.write_ctrl_u32(ctrl_input_write(s), 0);
            self.write_ctrl_u32(ctrl_input_read(s),  0);
        }
    }

    pub fn set_display_info(&mut self, width: u32, height: u32, hz: u32) {
        self.write_ctrl_u32(CTRL_DISPLAY_W, width);
        self.write_ctrl_u32(CTRL_DISPLAY_H, height);
        self.write_ctrl_u32(CTRL_REFRESH_HZ, hz);
    }

    pub fn set_system_font_hash(&mut self, hash: &[u8; 32]) {
        self.write_bytes(CTRL_SYSTEM_FONT, hash);
    }

    /// Command ring view scoped to a single client slot.
    pub fn command_ring(&mut self, slot: usize) -> CommandRing<'_> {
        debug_assert!(slot < NUM_CLIENT_SLOTS);
        CommandRing { link: self, slot }
    }

    /// Completion ring view scoped to a single client slot.
    pub fn completion_ring(&mut self, slot: usize) -> CompletionRing<'_> {
        debug_assert!(slot < NUM_CLIENT_SLOTS);
        CompletionRing { link: self, slot }
    }

    /// Input-ring view scoped to a single client slot. Server picks
    /// the slot per event based on the target window's owner.
    pub fn input_ring(&mut self, slot: usize) -> InputRing<'_> {
        debug_assert!(slot < NUM_CLIENT_SLOTS);
        InputRing { link: self, slot }
    }

    /// Snapshot of the kmod's live-slot bitmap. Each bit corresponds
    /// to one client slot.
    pub fn slots_alive_mask(&self) -> u32 {
        self.read_ctrl_u32(CTRL_SLOTS_ALIVE_MASK)
    }

    pub fn peek_cmd_write(&self, slot: usize) -> u32 {
        self.read_ctrl_u32(ctrl_cmd_write(slot))
    }
    pub fn peek_cmd_read(&self, slot: usize) -> u32 {
        self.read_ctrl_u32(ctrl_cmd_read(slot))
    }

    /// Read `len` bytes from a client slot's staging region.
    /// Returns None if the request exceeds the slot's staging window.
    pub fn read_slot_staging(&self, slot: usize, offset: usize, len: usize) -> Option<&[u8]> {
        if offset.checked_add(len)? > SLOT_STAGING_SIZE { return None; }
        let base = slot_staging_offset(slot) + offset;
        Some(&self.mmap[base..base + len])
    }

    fn read_ctrl_u32(&self, offset: usize) -> u32 {
        unsafe {
            let ptr = self.mmap.as_ptr().add(offset) as *const u32;
            core::ptr::read_volatile(ptr)
        }
    }

    fn write_ctrl_u32(&mut self, offset: usize, val: u32) {
        unsafe {
            let ptr = self.mmap.as_mut_ptr().add(offset) as *mut u32;
            core::ptr::write_volatile(ptr, val);
        }
        let _ = self.mmap.flush();
    }

    fn read_bytes(&self, offset: usize, len: usize) -> Vec<u8> {
        self.mmap[offset..offset + len].to_vec()
    }

    fn write_bytes(&mut self, offset: usize, data: &[u8]) {
        unsafe {
            let ptr = self.mmap.as_mut_ptr().add(offset);
            for (i, &b) in data.iter().enumerate() {
                core::ptr::write_volatile(ptr.add(i), b);
            }
        }
        let _ = self.mmap.flush();
    }
}

pub struct CommandRing<'a> {
    link: &'a mut IvshmemLink,
    slot: usize,
}

impl<'a> CommandRing<'a> {
    pub fn dequeue(&mut self) -> Option<Command> {
        let write_ptr = self.link.read_ctrl_u32(ctrl_cmd_write(self.slot));
        let read_ptr  = self.link.read_ctrl_u32(ctrl_cmd_read(self.slot));

        if read_ptr == write_ptr {
            return None;
        }

        let index = (read_ptr as usize) % CMD_RING_ENTRIES;
        let offset = slot_cmd_offset(self.slot) + index * CMD_ENTRY_SIZE;
        let entry = self.link.read_bytes(offset, CMD_ENTRY_SIZE);
        let cmd: Command = *bytemuck::from_bytes(&entry);

        self.link.write_ctrl_u32(ctrl_cmd_read(self.slot), read_ptr.wrapping_add(1));
        Some(cmd)
    }
}

pub struct CompletionRing<'a> {
    link: &'a mut IvshmemLink,
    slot: usize,
}

impl<'a> CompletionRing<'a> {
    pub fn enqueue(&mut self, comp: &Completion) {
        let write_ptr = self.link.read_ctrl_u32(ctrl_comp_write(self.slot));
        let index = (write_ptr as usize) % CMD_RING_ENTRIES;
        let offset = slot_comp_offset(self.slot) + index * CMD_ENTRY_SIZE;

        self.link.write_bytes(offset, bytemuck::bytes_of(comp));
        self.link.write_ctrl_u32(ctrl_comp_write(self.slot), write_ptr.wrapping_add(1));
    }
}

pub struct InputRing<'a> {
    link: &'a mut IvshmemLink,
    slot: usize,
}

impl<'a> InputRing<'a> {
    /// Enqueue an input event onto this slot's input ring. If the
    /// ring is full (the client isn't draining), drop the event.
    pub fn enqueue(&mut self, evt: &InputEvent) {
        let write_ptr = self.link.read_ctrl_u32(ctrl_input_write(self.slot));
        let read_ptr  = self.link.read_ctrl_u32(ctrl_input_read(self.slot));
        if write_ptr.wrapping_sub(read_ptr) >= INPUT_RING_ENTRIES as u32 {
            return;
        }
        let index = (write_ptr as usize) % INPUT_RING_ENTRIES;
        let offset = slot_input_offset(self.slot) + index * INPUT_ENTRY_SIZE;
        self.link.write_bytes(offset, bytemuck::bytes_of(evt));
        self.link.write_ctrl_u32(ctrl_input_write(self.slot),
            write_ptr.wrapping_add(1));
    }
}
