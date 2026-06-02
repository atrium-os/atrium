//! Carillon T0 loopback: host endpoint + a fake guest, no QEMU, no kmod.
//!
//! The host maps a shared-memory file and runs its serve-loop blocked on
//! the guest→host doorbell; a "guest" thread opens the *same* file
//! (independent mapping → shared physical pages) and drives the
//! reference ring protocol. Proves the round-trip (submission ring →
//! handler → completion ring) and the no-spin invariant: the host wakes
//! exactly once per doorbell and batch-drains everything visible.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct_gpu_host::carillon::{
    layout, CompDesc, Doorbell, GuestRing, Host, Region, SubDesc,
};

fn tmp_shm(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "carillon-loop-{}-{}.shm",
        std::process::id(),
        name
    ))
}

/// Spin up host + guest over a shared file and two doorbells. Returns
/// the guest handle, a join handle for the host, the host's final
/// wakeup count (via a shared cell), and a stop closure.
struct Rig {
    guest: GuestRing,
    host_join: Option<thread::JoinHandle<(u64, u64)>>,
    path: PathBuf,
}

impl Rig {
    fn start(name: &str) -> Self {
        let path = tmp_shm(name);
        let host_region = Region::create(&path, layout::TOTAL_SIZE).unwrap();

        // Two doorbells: g2h (host waits) and h2g (host signals).
        let g2h = Doorbell::new().unwrap();
        let h2g = Doorbell::new().unwrap();
        let mut host = Host::new(host_region, g2h, h2g);
        // Guest-side fd copies (Host owns/closes the Doorbells).
        let (g2h_write_fd, h2g_read_fd) = host.guest_doorbell_fds();

        // Guest opens the SAME file → its own mapping of shared pages.
        let guest_region = Region::open(&path, layout::TOTAL_SIZE).unwrap();
        guest_region.validate_header().unwrap();
        let guest = GuestRing::new(guest_region, g2h_write_fd, h2g_read_fd);

        let host_join = thread::spawn(move || {
            // Echo handler: ack each frame, carrying the fence id +
            // frame ref into the completion.
            host.serve(|sub: &SubDesc| CompDesc {
                kind: CompDesc::KIND_FRAME_DONE,
                fence_id: sub.fence_id,
                result: 0,
                readback_off: sub.frame_off,
                readback_len: sub.frame_len,
            })
            .unwrap();
            (host.wakeups(), host.frames_processed())
        });

        Rig { guest, host_join: Some(host_join), path }
    }

    /// Send a STOP descriptor + ring, then join the host thread and
    /// return (wakeups, frames_processed).
    fn stop_and_join(mut self) -> (u64, u64) {
        self.guest.submit(&SubDesc { kind: SubDesc::KIND_STOP, ..Default::default() });
        self.guest.ring();
        let res = self.host_join.take().unwrap().join().unwrap();
        let _ = std::fs::remove_file(&self.path);
        res
    }
}

#[test]
fn batch_drained_in_one_wake() {
    let rig = Rig::start("batch");

    // Submit three frames, then ring ONCE. The host must wake a single
    // time and drain all three (coalesced) — the no-spin/batch property.
    for i in 1..=3u32 {
        rig.guest.submit(&SubDesc {
            kind: SubDesc::KIND_FRAME,
            fence_id: i,
            frame_off: 0x1000 * i,
            frame_len: 64 * i,
            flags: 0,
        });
    }
    rig.guest.ring();

    // One completion-doorbell wake delivers all three completions.
    let comps = rig.guest.wait_completions().unwrap();
    assert_eq!(comps.len(), 3, "all three frames complete on one batch");
    for (j, c) in comps.iter().enumerate() {
        let i = j as u32 + 1;
        assert_eq!(c.kind, CompDesc::KIND_FRAME_DONE);
        assert_eq!(c.fence_id, i, "fence id echoed in order");
        assert_eq!(c.readback_off, 0x1000 * i, "frame ref carried through");
        assert_eq!(c.readback_len, 64 * i);
        assert_eq!(c.result, 0);
    }

    let (wakeups, frames) = rig.stop_and_join();
    // The 3 frames were drained in ONE wake; the STOP added a 2nd wake.
    assert_eq!(frames, 3, "exactly three frames processed");
    assert_eq!(wakeups, 2, "one wake for the batch + one for STOP — no spin");
}

#[test]
fn pingpong_roundtrip() {
    let rig = Rig::start("pingpong");

    // 5 strict round-trips: submit one, ring, park for its completion.
    for i in 1..=5u32 {
        rig.guest.submit(&SubDesc {
            kind: SubDesc::KIND_FRAME,
            fence_id: 100 + i,
            frame_off: i,
            frame_len: 1,
            flags: 0,
        });
        rig.guest.ring();
        let comps = rig.guest.wait_completions().unwrap();
        assert_eq!(comps.len(), 1, "one completion per round-trip");
        assert_eq!(comps[0].fence_id, 100 + i);
    }

    let (wakeups, frames) = rig.stop_and_join();
    assert_eq!(frames, 5);
    // 5 per-frame wakes + 1 STOP wake. Never more — proves the host
    // slept between frames rather than polling.
    assert_eq!(wakeups, 6, "exactly one wake per frame + STOP — no spin");
}

#[test]
fn fire_and_forget_does_not_block_submitter() {
    // fence_id == 0 frames: the guest submits without ever parking on a
    // completion. The host still processes them; we observe via a shared
    // counter that the submitter returned promptly (well under any wall
    // the host would impose if it serialized).
    let rig = Rig::start("faf");
    let progressed = Arc::new(AtomicU64::new(0));

    let t0 = std::time::Instant::now();
    for _ in 0..16 {
        rig.guest.submit(&SubDesc { kind: SubDesc::KIND_FRAME, ..Default::default() });
        progressed.fetch_add(1, Ordering::Relaxed);
    }
    rig.guest.ring();
    let submit_elapsed = t0.elapsed();
    assert_eq!(progressed.load(Ordering::Relaxed), 16);
    assert!(submit_elapsed < Duration::from_millis(50),
        "fire-and-forget submit must not block on completion (took {submit_elapsed:?})");

    // Drain whatever completed (host acks even fence_id==0 frames).
    thread::sleep(Duration::from_millis(20));
    let _ = rig.guest.drain_completions();

    let (_, frames) = rig.stop_and_join();
    assert_eq!(frames, 16);
}
