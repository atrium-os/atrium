//! `fenestra-stream-replay [--viewers N] [--drop-acks P] [--seed S] REC...`:
//! play frescod session recordings (`fresco_render_core::record`) through
//! the scene stream and check that every viewer packs every window to the
//! same bytes as the host. Prints one line per recording and exits 1 if
//! any failed (fenestra.md §19, R1; `scripts/fenestra-stream-check.sh`).

use fresco_render_core::record;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut viewers, mut drop_acks, mut seed) = (2usize, 0.3f64, 1u64);
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        let mut val = |name: &str| args.next().unwrap_or_else(|| {
            eprintln!("fenestra-stream-replay: {name} needs a value");
            std::process::exit(2)
        });
        match a.as_str() {
            "--viewers" => viewers = val("--viewers").parse().unwrap_or(2).clamp(1, 8),
            "--drop-acks" => drop_acks = val("--drop-acks").parse().unwrap_or(0.3f64).clamp(0.0, 1.0),
            "--seed" => seed = val("--seed").parse().unwrap_or(1),
            "-h" | "--help" => {
                println!("usage: fenestra-stream-replay [--viewers N] [--drop-acks P] [--seed S] REC...");
                return;
            }
            _ => files.push(a),
        }
    }
    if files.is_empty() {
        eprintln!("fenestra-stream-replay: no recordings given");
        std::process::exit(2);
    }
    let mut failed = false;
    for f in &files {
        let events = match std::fs::File::open(f).and_then(record::read) {
            Ok(e) => e,
            Err(e) => {
                println!("FAIL {f}: {e}");
                failed = true;
                continue;
            }
        };
        let r = record::replay(&events, viewers, drop_acks, seed);
        let verdict = if r.passed() { "PASS" } else { "FAIL" };
        println!(
            "{verdict} {f}: {} events, {} draws, {} windows, {} frames compared, {} differed, {} viewer errors, {} frames sent ({} whole), {} bytes",
            r.events, r.draws, r.windows, r.compared, r.differed.len(), r.viewer_errors.len(), r.frames_sent, r.whole_sent, r.bytes_sent
        );
        for d in r.differed.iter().chain(&r.viewer_errors).take(10) {
            println!("  {d}");
        }
        failed |= !r.passed();
    }
    std::process::exit(i32::from(failed));
}
