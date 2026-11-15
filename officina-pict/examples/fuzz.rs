//! Mutation fuzzing over a directory of pictures: `fuzz DIR [ROUNDS]`.
//! Each round flips, copies or zeroes a few bytes of one file and draws
//! it at 400 × 300; reports the slowest draw. A panic is the failure.

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("fuzz DIR [ROUNDS]");
    let rounds: usize = args.next().map_or(20_000, |r| r.parse().unwrap());
    let files: Vec<Vec<u8>> = std::fs::read_dir(&dir).unwrap().filter_map(|e| std::fs::read(e.ok()?.path()).ok()).filter(|b| officina_pict::sniff(b).is_some()).collect();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut worst = (std::time::Duration::ZERO, 0);
    for i in 0..rounds {
        let mut b = files[rnd() as usize % files.len()].clone();
        for _ in 0..1 + rnd() % 8 {
            let at = rnd() as usize % b.len();
            match rnd() % 4 {
                0 => b[at] ^= 1 << (rnd() % 8),
                1 => b[at] = rnd() as u8,
                2 => {
                    let n = (rnd() % 8) as usize;
                    for k in at..(at + n).min(b.len()) {
                        b[k] = 0xFF;
                    }
                }
                _ => {
                    let from = rnd() as usize % b.len();
                    let n = (rnd() % 16) as usize;
                    for k in 0..n {
                        if at + k < b.len() && from + k < b.len() {
                            b[at + k] = b[from + k];
                        }
                    }
                }
            }
        }
        let t = std::time::Instant::now();
        let _ = officina_pict::natural_size(&b);
        if std::panic::catch_unwind(|| officina_pict::render(&b, 400, 300)).is_err() {
            std::fs::write(format!("/tmp/claude-0/-home-user/83a9d2e7-16cb-5a72-8226-e0fdfe16d338/scratchpad/crash-{i}"), &b).unwrap();
            std::process::exit(1);
        }
        let e = t.elapsed();
        if e > worst.0 {
            worst = (e, i);
        }
    }
    println!("{rounds} rounds over {} files; slowest {:?} (round {})", files.len(), worst.0, worst.1);
}
