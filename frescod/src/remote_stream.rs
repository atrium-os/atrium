//! A renderer's scene streams, for Fenestra (fenestra.md §3.2, §5.2).
//!
//! While the broker has a remote session (`Msg::Remote`), each of this
//! client's windows keeps a stream [`Writer`]: after every draw pass it
//! notes what changed and tells the broker (`STREAM_CHANGED`). The broker
//! relays a viewer's want (`STREAM_WANT`: what that viewer holds); the
//! renderer writes the canonical difference from it, which it serialises
//! from its own validated scene, never from the client's bytes, through
//! two stream slots the broker made (`STREAM_DATA`, then `STREAM_END`).
//! Wants are answered one at a time, in order.

use std::collections::{HashMap, VecDeque};

use fresco_render_core::pack::SceneState;
use fresco_render_core::stream::Writer;
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_split::chan::Shm;
use fresco_split::Msg;

/// Most wants the broker may have outstanding; past this it is broken.
pub const MAX_WANTS: usize = 256;
/// The largest stream slot this renderer maps.
pub const MAX_SLOT_BYTES: u32 = 64 << 20;

type Version = (u64, u64);

struct Out {
    window: u32,
    ticket: u32,
    bytes: Vec<u8>,
    at: usize,
    now: Option<Version>,
}

#[derive(Default)]
pub struct Remote {
    slots: Option<[Shm; 2]>,
    busy: [bool; 2],
    writers: HashMap<u32, Writer>,
    wants: VecDeque<(u32, u32, Option<Version>)>,
    out: Option<Out>,
    /// Makes each writer's epoch unique to this process.
    epoch_base: u64,
}

fn fresh_u64() -> u64 {
    let mut b = [0u8; 8];
    // SAFETY: getrandom writes at most `b.len()` bytes into our buffer.
    let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    if n == b.len() as isize {
        u64::from_le_bytes(b)
    } else {
        // Epochs need only differ between processes: the clock and pid do.
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: one timespec of ours for the call.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut t) };
        (t.tv_sec as u64).wrapping_mul(1_000_000_007) ^ (t.tv_nsec as u64) ^ ((std::process::id() as u64) << 40)
    }
}

impl Remote {
    pub fn on(&self) -> bool {
        self.slots.is_some()
    }

    /// The broker turned streaming on (with its two slots) or off. Either
    /// way every writer starts again, so no viewer is sent a difference
    /// from a version of a writer this process no longer has.
    pub fn set(&mut self, slots: Option<[Shm; 2]>) {
        self.slots = slots;
        self.busy = [false; 2];
        self.writers.clear();
        self.wants.clear();
        self.out = None;
        self.epoch_base = fresh_u64();
    }

    pub fn want(&mut self, window: u32, ticket: u32, base: Option<Version>) -> Result<(), &'static str> {
        if !self.on() {
            return Err("STREAM_WANT with no remote session");
        }
        if self.wants.len() >= MAX_WANTS {
            return Err("too many STREAM_WANTs outstanding");
        }
        self.wants.push_back((window, ticket, base));
        Ok(())
    }

    pub fn done(&mut self, slot: u32) -> Result<(), &'static str> {
        match self.busy.get_mut(slot as usize) {
            Some(b) if *b => {
                *b = false;
                Ok(())
            }
            _ => Err("STREAM_DONE for a slot not in use"),
        }
    }

    /// After a draw pass: bring each window's writer to its scene, tell
    /// the broker which changed, then answer wants while a slot is free.
    /// `windows` are this client's windows with their sizes; one in the
    /// middle of a client frame is left out (its state is half made).
    pub fn step(&mut self, fe: &EnvelopeFrontend, scene: &SceneState, windows: &[(u32, (u32, u32))], send: &mut dyn FnMut(&Msg)) {
        if !self.on() {
            return;
        }
        self.writers.retain(|id, _| windows.iter().any(|(w, _)| w == id) || fe.window_state(*id).is_some());
        for &(id, size) in windows {
            let Some(st) = fe.window_state(id) else { continue };
            let base = self.epoch_base;
            let w = self.writers.entry(id).or_insert_with(|| Writer::new(base ^ (u64::from(id) << 48)));
            if w.update(st, scene, size) {
                send(&Msg::StreamChanged { window: id });
            }
        }
        loop {
            if self.out.is_none() {
                let Some((window, ticket, base)) = self.wants.pop_front() else { return };
                let mut bytes = Vec::new();
                let now = match (self.writers.get(&window), fe.window_state(window)) {
                    (Some(w), Some(st)) => {
                        w.diff(st, scene, base, &mut bytes);
                        Some((w.epoch(), w.version()))
                    }
                    _ => None,
                };
                self.out = Some(Out { window, ticket, bytes, at: 0, now });
            }
            let Some(o) = self.out.as_mut() else { return };
            if o.at >= o.bytes.len() {
                send(&Msg::StreamEnd { window: o.window, ticket: o.ticket, now: o.now });
                self.out = None;
                continue;
            }
            let Some(slots) = self.slots.as_mut() else { return };
            let Some(i) = (0..2).find(|&i| !self.busy[i]) else { return }; // STREAM_DONE wakes us
            let room = slots[i].len();
            let n = (o.bytes.len() - o.at).min(room);
            slots[i].as_mut_slice()[..n].copy_from_slice(&o.bytes[o.at..o.at + n]);
            o.at += n;
            self.busy[i] = true;
            send(&Msg::StreamData { window: o.window, ticket: o.ticket, slot: i as u32, len: n as u32 });
        }
    }
}
