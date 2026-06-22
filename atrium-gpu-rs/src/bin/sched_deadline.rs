//! sched_deadline — deadline-aware GPU scheduling through the real driver ABI
//! (atrium-gpu-scheduler §6, the in-VM wiring). Plays a compositor (a queue with
//! a near vblank deadline) racing a background GPU hog for the firmware scheduler.
//! With a deadline window set and the compositor's deadline inside it, the
//! scheduler serves the compositor decisively — it makes its frame instead of
//! slipping behind the hog. This is the GPU half of frame-pacing-under-contention,
//! driven the way frescod would stamp the compositor's queue each frame.
//!
//! Run on a fresh boot (the firmware scheduler accumulates queues across runs).

use atrium_gpu::amd::Gpu;
use std::io;

fn main() {
    match run() {
        Ok(()) => println!("ALL OK"),
        Err(e) => {
            eprintln!("FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> io::Result<()> {
    let gpu = Gpu::open()?;

    // Two identical ~1 ms VRAM queues (weight 1; 640 MB at 640 GB/s ≈ 1 ms/round):
    // queue indices are count-1 as they're appended.
    let comp = gpu.sched_add_queue(1, 1, 640_000_000, 3)? - 1; // the compositor
    let bg = gpu.sched_add_queue(1, 1, 640_000_000, 3)? - 1;   // a background hog

    // A 10 ms deadline window; stamp the compositor's queue with a 5 ms deadline
    // (inside the window) — as a broker (frescod) would with the target vblank.
    gpu.sched_set_window(10_000_000)?;
    gpu.sched_set_deadline(comp, 5_000_000)?;

    // Run scheduling rounds; the firmware picks deadline-aware.
    gpu.sched_run(20)?;

    let (c_runs, c_us, _) = gpu.sched_query(comp)?;
    let (b_runs, b_us, _) = gpu.sched_query(bg)?;
    eprintln!("compositor (deadline 5 ms): {c_runs} rounds, {c_us} µs engine time");
    eprintln!("background (no deadline):   {b_runs} rounds, {b_us} µs engine time");

    if c_runs <= b_runs.saturating_mul(4).max(4) {
        return Err(io::Error::other(format!(
            "compositor not served decisively near its deadline ({c_runs} vs {b_runs})"
        )));
    }
    eprintln!(
        "deadline-aware scheduling confirmed: the compositor commanded the GPU near its vblank \
         (would have split ~50/50 deadline-blind) — it makes its frame, the hog yields"
    );
    Ok(())
}
