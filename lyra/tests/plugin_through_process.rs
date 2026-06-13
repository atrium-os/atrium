//! End-to-end: a C plugin hosted in the isolated lyra-effect process, driven
//! through the real shared-memory rings — the full ambition-1+3 path on the host
//! (lyrad → ring → separate process → dlopen'd C node → ring back).

use lyra::ring::Ring;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const CH: u64 = 2;

fn build_tremolo() -> PathBuf {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join("lyra_tremolo_e2e.so");
    let status = Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            "-O2",
            &format!("-I{crate_dir}/include"),
            &format!("{crate_dir}/plugins/tremolo.c"),
            "-lm",
            "-o",
        ])
        .arg(&out)
        .status()
        .expect("cc available");
    assert!(status.success());
    out
}

#[test]
fn c_plugin_modulates_audio_through_the_isolated_process() {
    let so = build_tremolo();
    // unique ring names so concurrent test runs don't collide.
    let pid = std::process::id();
    let dry_name = format!("/lyra_e2e_dry_{pid}");
    let wet_name = format!("/lyra_e2e_wet_{pid}");
    let dry = Ring::create(&dry_name, 4096, CH).expect("dry ring"); // parent writes
    // wet: create owns the shm (unlinks on drop); the child opens it as producer
    // and writes; the parent reads through a consumer handle.
    let _wet_owner = Ring::create(&wet_name, 4096, CH).expect("wet ring");
    let wet = Ring::open(&wet_name, false).expect("wet consumer");

    // spawn the effect node hosting the C plugin (trem_hz=0 so the *built-in*
    // path would be unity passthrough — any modulation we see is the C node).
    let bin = env!("CARGO_BIN_EXE_lyra-effect");
    let mut child = Command::new(bin)
        .args([&dry_name, &wet_name, "0", "--plugin"])
        .arg(&so)
        .spawn()
        .expect("spawn lyra-effect");

    // feed a DC signal in; collect the processed output out.
    let frame = [1.0f32, 1.0f32];
    let mut fed = 0usize;
    let mut got: Vec<f32> = Vec::new();
    let mut rbuf = vec![0.0f32; 256 * CH as usize];
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < 12000 * CH as usize && Instant::now() < deadline {
        while fed < 16000 {
            if dry.write(&frame) == 0 {
                break; // ring full; let the consumer drain
            }
            fed += 1;
        }
        let n = wet.read(&mut rbuf) as usize;
        if n > 0 {
            got.extend_from_slice(&rbuf[..n * CH as usize]);
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    assert!(got.len() >= 12000 * CH as usize, "got {} floats back", got.len());
    let max = got.iter().cloned().fold(f32::MIN, f32::max);
    let min = got.iter().cloned().fold(f32::MAX, f32::min);
    // the C tremolo scales the DC by a 0..1 LFO: bounded, and genuinely varying.
    assert!(max <= 1.0001 && min >= -0.0001, "in gain range [{min},{max}]");
    assert!(max - min > 0.3, "the C node actually modulated it: {}", max - min);
}
