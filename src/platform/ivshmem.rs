use crate::command::protocol::{Command, Completion};
use crate::input::capture::InputEvent;

use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::Path;

const CTRL_OFFSET: usize = 0x0000;
const CMD_RING_OFFSET: usize = 0x1000;       // 256 × 128 = 32KB
const COMP_RING_OFFSET: usize = 0x9000;      // after cmd ring
const INPUT_RING_OFFSET: usize = 0x11000;    // after comp ring
const STAGING_OFFSET: usize = 0x12000;

const CMD_RING_ENTRIES: usize = 256;
const CMD_ENTRY_SIZE: usize = 128;
const INPUT_RING_ENTRIES: usize = 64;
const INPUT_ENTRY_SIZE: usize = 64;

// Control register layout (64 bytes — phase B1 reserves up to 128)
// [0:3]   cmd_write_ptr   (guest writes)
// [4:7]   cmd_read_ptr    (server writes)
// [8:11]  comp_write_ptr  (server writes)
// [12:15] comp_read_ptr   (guest writes)
// [16:19] input_write_ptr (server writes)
// [20:23] input_read_ptr  (guest writes)
// [24:27] status          (server writes)
// [28:31] display_width   (server writes)
// [32:35] display_height  (server writes)
// [36:39] refresh_hz      (server writes)
// [40:71] system font hash (server writes, 32 bytes)
// [72:75] current_window  (guest writes — slot/frame ops apply to this window)

const CTRL_CMD_WRITE: usize = 0;
const CTRL_CMD_READ: usize = 4;
const CTRL_COMP_WRITE: usize = 8;
const CTRL_COMP_READ: usize = 12;
const CTRL_INPUT_WRITE: usize = 16;
const CTRL_INPUT_READ: usize = 20;
const CTRL_STATUS: usize = 24;
const CTRL_DISPLAY_W: usize = 28;
const CTRL_DISPLAY_H: usize = 32;
const CTRL_REFRESH_HZ: usize = 36;
const CTRL_SYSTEM_FONT: usize = 40; // 32 bytes: SHA256 of system font blob
#[allow(dead_code)]
const CTRL_CURRENT_WINDOW: usize = 72; // u32 — phase B1, used only when present

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

        // zero all control and ring regions on startup
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
        self.write_ctrl_u32(CTRL_CMD_WRITE, 0);
        self.write_ctrl_u32(CTRL_CMD_READ, 0);
        self.write_ctrl_u32(CTRL_COMP_WRITE, 0);
        self.write_ctrl_u32(CTRL_COMP_READ, 0);
        self.write_ctrl_u32(CTRL_INPUT_WRITE, 0);
        self.write_ctrl_u32(CTRL_INPUT_READ, 0);
    }

    pub fn set_display_info(&mut self, width: u32, height: u32, hz: u32) {
        self.write_ctrl_u32(CTRL_DISPLAY_W, width);
        self.write_ctrl_u32(CTRL_DISPLAY_H, height);
        self.write_ctrl_u32(CTRL_REFRESH_HZ, hz);
    }

    pub fn set_system_font_hash(&mut self, hash: &[u8; 32]) {
        self.write_bytes(CTRL_SYSTEM_FONT, hash);
    }

    pub fn command_ring(&mut self) -> CommandRing<'_> {
        CommandRing { link: self }
    }

    pub fn completion_ring(&mut self) -> CompletionRing<'_> {
        CompletionRing { link: self }
    }

    pub fn input_ring(&mut self) -> InputRing<'_> {
        InputRing { link: self }
    }

    fn read_ctrl_u32(&self, offset: usize) -> u32 {
        let base = CTRL_OFFSET + offset;
        unsafe {
            let ptr = self.mmap.as_ptr().add(base) as *const u32;
            core::ptr::read_volatile(ptr)
        }
    }

    fn write_ctrl_u32(&mut self, offset: usize, val: u32) {
        let base = CTRL_OFFSET + offset;
        unsafe {
            let ptr = self.mmap.as_mut_ptr().add(base) as *mut u32;
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
}

impl<'a> CommandRing<'a> {
    pub fn dequeue(&mut self) -> Option<Command> {
        let write_ptr = self.link.read_ctrl_u32(CTRL_CMD_WRITE);
        let read_ptr = self.link.read_ctrl_u32(CTRL_CMD_READ);

        if read_ptr == write_ptr {
            return None;
        }

        let index = (read_ptr as usize) % CMD_RING_ENTRIES;
        let offset = CMD_RING_OFFSET + index * CMD_ENTRY_SIZE;
        let entry = self.link.read_bytes(offset, CMD_ENTRY_SIZE);
        let cmd: Command = *bytemuck::from_bytes(&entry);

        self.link.write_ctrl_u32(CTRL_CMD_READ, read_ptr.wrapping_add(1));
        Some(cmd)
    }
}

pub struct CompletionRing<'a> {
    link: &'a mut IvshmemLink,
}

impl<'a> CompletionRing<'a> {
    pub fn enqueue(&mut self, comp: &Completion) {
        let write_ptr = self.link.read_ctrl_u32(CTRL_COMP_WRITE);
        let index = (write_ptr as usize) % CMD_RING_ENTRIES;
        let offset = COMP_RING_OFFSET + index * CMD_ENTRY_SIZE;

        self.link.write_bytes(offset, bytemuck::bytes_of(comp));
        self.link.write_ctrl_u32(CTRL_COMP_WRITE, write_ptr.wrapping_add(1));
    }
}

pub struct InputRing<'a> {
    link: &'a mut IvshmemLink,
}

impl<'a> InputRing<'a> {
    pub fn enqueue(&mut self, evt: &InputEvent) {
        let write_ptr = self.link.read_ctrl_u32(CTRL_INPUT_WRITE);
        let read_ptr = self.link.read_ctrl_u32(CTRL_INPUT_READ);

        if write_ptr.wrapping_sub(read_ptr) >= INPUT_RING_ENTRIES as u32 {
            return;
        }

        let index = (write_ptr as usize) % INPUT_RING_ENTRIES;
        let offset = INPUT_RING_OFFSET + index * INPUT_ENTRY_SIZE;

        self.link.write_bytes(offset, bytemuck::bytes_of(evt));
        self.link.write_ctrl_u32(CTRL_INPUT_WRITE, write_ptr.wrapping_add(1));
    }
}
