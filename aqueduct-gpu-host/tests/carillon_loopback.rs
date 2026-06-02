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
    layout, CompDesc, Doorbell, GuestRing, Host, Region, ShutdownHandle, SubDesc,
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
    shutdown: ShutdownHandle,
    host_join: Option<thread::JoinHandle<(u64, u64)>>,
    path: PathBuf,
}

impl Rig {
    /// Echo handler: ack each frame, carrying the fence id + frame ref
    /// into the completion (ignores the staged bytes).
    fn start(name: &str) -> Self {
        Rig::start_with(name, |sub: &SubDesc, _bytes: &[u8]| CompDesc {
            kind: CompDesc::KIND_FRAME_DONE,
            fence_id: sub.fence_id,
            result: 0,
            readback_off: sub.frame_off,
            readback_len: sub.frame_len,
        })
    }

    /// Start host + guest with a custom per-frame dispatcher.
    fn start_with<H>(name: &str, handler: H) -> Self
    where
        H: FnMut(&SubDesc, &[u8]) -> CompDesc + Send + 'static,
    {
        let path = tmp_shm(name);
        let host_region = Region::create(&path, layout::TOTAL_SIZE).unwrap();

        // Two doorbells: g2h (host waits) and h2g (host signals).
        let g2h = Doorbell::new().unwrap();
        let h2g = Doorbell::new().unwrap();
        let mut host = Host::new(host_region, g2h, h2g).unwrap();
        // Guest-side fd copies (Host owns/closes the Doorbells).
        let (g2h_write_fd, h2g_read_fd) = host.guest_doorbell_fds();
        let shutdown = host.shutdown_handle();

        // Guest opens the SAME file → its own mapping of shared pages.
        let guest_region = Region::open(&path, layout::TOTAL_SIZE).unwrap();
        guest_region.validate_header().unwrap();
        let guest = GuestRing::new(guest_region, g2h_write_fd, h2g_read_fd);

        let host_join = thread::spawn(move || {
            let mut handler = handler;
            host.serve(&mut handler).unwrap();
            (host.wakeups(), host.frames_processed())
        });

        Rig { guest, shutdown, host_join: Some(host_join), path }
    }

    /// Fire the out-of-band shutdown (kqueue/poll wakes on it, separate
    /// from the doorbell), then join the host thread and return
    /// (wakeups, frames_processed). Wakeups count doorbell wakes only.
    fn stop_and_join(mut self) -> (u64, u64) {
        self.shutdown.shutdown();
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
    // The 3 frames were drained in exactly ONE doorbell wake (shutdown is
    // a separate, uncounted kqueue event) — the batch / no-spin property.
    assert_eq!(frames, 3, "exactly three frames processed");
    assert_eq!(wakeups, 1, "all three drained in one doorbell wake — no spin");
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
    // Exactly one doorbell wake per frame, never more — proves the host
    // slept between frames rather than polling.
    assert_eq!(wakeups, 5, "exactly one doorbell wake per frame — no spin");
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

#[test]
fn frame_through_carillon_renders_on_backend() {
    // The transport's real job: a guest stages a real aqueduct-gpu
    // FrameOp stream in the arena and submits a descriptor referencing
    // it; the host drains it and dispatches the bytes to a Backend that
    // actually renders. Proves Carillon carries the live wire end to end
    // (host-side, SoftwareBackend → CI-portable, no MoltenVk/DYLD).
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::ids::{IdNamespace, ResourceId};
    use aqueduct_gpu::opcodes::FrameOp;
    use aqueduct_gpu_host::software::{
        BeginRenderPassBody, RectOpParams, BUILTIN_PIPELINE_RECT,
    };
    use aqueduct_gpu_host::{Backend, SoftwareBackend};

    const W: u32 = 64;
    const H: u32 = 64;

    let sw = Arc::new(SoftwareBackend::new());
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 1);
    sw.image_created(image_id, W, H);

    // Host per-frame dispatcher: run the staged FrameOp stream through
    // the backend (this is the seam the daemon's Session fills in T3).
    let sw_host = sw.clone();
    let rig = Rig::start_with("sw_frame", move |sub: &SubDesc, bytes: &[u8]| {
        let fence = ResourceId::new(IdNamespace::IcdRuntime, sub.fence_id);
        let ok = sw_host.submit_frame(fence, 1, bytes);
        CompDesc {
            kind: if ok { CompDesc::KIND_FRAME_DONE } else { CompDesc::KIND_ERROR },
            fence_id: sub.fence_id,
            result: u32::from(!ok),
            readback_off: 0,
            readback_len: W * H * 4,
        }
    });

    // Build a cyan-rect frame (builtin rect pipeline — no SPIR-V upload).
    let mut fb = FrameBuilder::new(8192);
    fb.push(
        FrameOp::BeginRenderPass,
        &BeginRenderPassBody {
            target_image_id: image_id.raw(),
            clear_color_rgba8: [0, 0, 0, 255],
            flags: 0,
        }
        .to_bytes(),
    )
    .unwrap();
    let rect_pipe = ResourceId::new(IdNamespace::Builtin, BUILTIN_PIPELINE_RECT);
    fb.push(FrameOp::BindPipeline, &rect_pipe.raw().to_le_bytes()).unwrap();
    let params = RectOpParams {
        x: 0.0, y: 0.0, w: W as f32, h: H as f32,
        r: 0.0, g: 1.0, b: 1.0, a: 1.0, // cyan
    };
    let mut pc = vec![0u8; 4];
    pc.extend_from_slice(&params.to_bytes());
    fb.push(FrameOp::PushConstants, &pc).unwrap();
    fb.push(FrameOp::Draw, &[0u8; 16]).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    let frame = fb.as_bytes().to_vec();

    // Stage in the arena, submit a descriptor referencing it, ring.
    rig.guest.stage_frame(0, &frame);
    rig.guest.submit(&SubDesc {
        kind: SubDesc::KIND_FRAME,
        fence_id: 1,
        frame_off: 0,
        frame_len: frame.len() as u32,
        flags: 0,
    });
    rig.guest.ring();

    let comps = rig.guest.wait_completions().unwrap();
    assert_eq!(comps.len(), 1);
    assert_eq!(comps[0].kind, CompDesc::KIND_FRAME_DONE, "frame rendered ok");

    // SoftwareBackend renders synchronously inside submit_frame, so the
    // completion implies pixels are ready. cyan @ full alpha → (0,255,255,255).
    let px = sw.read_image_pixels(image_id).expect("rendered pixels");
    let c = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
    assert_eq!(
        &px[c..c + 4],
        &[0, 255, 255, 255],
        "cyan centre rendered via the Carillon transport"
    );

    let (_, frames) = rig.stop_and_join();
    assert_eq!(frames, 1);
}
