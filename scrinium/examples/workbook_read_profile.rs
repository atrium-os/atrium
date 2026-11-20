//! Profile a writable, private 64 MiB workbook. All times are elapsed microseconds.
use std::fs::File;
use std::hint::black_box;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

const CHUNK: usize = 64 * 1024;
const SIZE: usize = 64 * 1024 * 1024;

fn sample(bytes: &[u8]) -> u64 {
    bytes.chunks(CHUNK).map(|chunk| u64::from(chunk[0])).sum()
}

fn main() -> io::Result<()> {
    let path = std::env::args_os().nth(1).expect("workbook path");
    let path = Path::new(&path);
    let file = File::options().read(true).write(true).open(path)?;
    assert_eq!(file.metadata()?.len(), SIZE as u64);
    let resident = std::fs::read(path)?;
    let expected = blake3::hash(&resident);
    let expected_sample = sample(&resident);
    eprintln!("bytes={SIZE} blake3={expected}");
    println!("round,case,total_us,read_us,hash_us,read_q1_us,read_q2_us,read_q3_us,read_q4_us");
    let cases = [
        "pread64",
        "hash64",
        "split64",
        "grant",
        "pread1024",
        "sequential64",
    ];
    // One warm-up round, then each case occupies each position twice.
    for round in 0..=12 {
        for step in 0..cases.len() {
            let case = cases[(round + step) % cases.len()];
            let size = if case == "pread1024" {
                16 * CHUNK
            } else {
                CHUNK
            };
            let mut buffer = vec![0u8; size];
            // Give the sequential case a fresh offset without affecting the pinned fd.
            let mut sequential = File::open(path)?;
            let mut hash = blake3::Hasher::new();
            let mut hashed = None;
            let mut witness = 0;
            let mut read = [Duration::ZERO; 4];
            let mut hash_time = Duration::ZERO;
            let start = Instant::now();
            match case {
                "grant" => {
                    black_box(scrinium::workbook::grant(path, &file)?);
                }
                "hash64" => {
                    for chunk in resident.chunks_exact(CHUNK) {
                        hash.update(chunk);
                    }
                    hashed = Some(hash.finalize());
                }
                "split64" => {
                    for offset in (0..SIZE).step_by(CHUNK) {
                        let before = Instant::now();
                        file.read_exact_at(&mut buffer, offset as u64)?;
                        let middle = Instant::now();
                        hash.update(&buffer);
                        let after = Instant::now();
                        read[offset / (SIZE / 4)] += middle - before;
                        hash_time += after - middle;
                    }
                    let before = Instant::now();
                    hashed = Some(hash.finalize());
                    hash_time += before.elapsed();
                }
                "pread64" | "pread1024" | "sequential64" => {
                    for offset in (0..SIZE).step_by(size) {
                        if case == "sequential64" {
                            sequential.read_exact(&mut buffer)?;
                        } else {
                            file.read_exact_at(&mut buffer, offset as u64)?;
                        }
                        witness += sample(black_box(&buffer));
                    }
                }
                _ => unreachable!(),
            }
            let total = start.elapsed();
            if let Some(digest) = hashed {
                assert_eq!(digest, expected);
            }
            if matches!(case, "pread64" | "pread1024" | "sequential64") {
                assert_eq!(witness, expected_sample);
            }
            let us = |v: Duration| v.as_micros();
            let [a, b, c, d] = read.map(us);
            println!(
                "{round},{case},{},{},{},{a},{b},{c},{d}",
                us(total),
                us(read.iter().sum()),
                us(hash_time)
            );
        }
    }
    assert_eq!(blake3::hash(&std::fs::read(path)?), expected);
    Ok(())
}
