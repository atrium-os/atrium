//! `fenestra-view --headless --mint-dir DIR --out DIR [--run-secs N] [--reconnect-after S] [--connect ADDR] [--idle-after S] [--input FILE]`
//!
//! Headless (gates, §14.2): connect with the mint in `--mint-dir`
//! (`fenestra_wire::mint`; it leaves its public key there first), draw the
//! host's windows for `--run-secs`, then write `window-ID.png` and
//! `viewer.txt` to `--out`. `--reconnect-after S` drops the connection
//! after S seconds and connects again, resuming from what it holds.
//! A windowed mode (winit) comes next.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fenestra_view::{run, snapshot, Views};
use fenestra_wire::scene::Input;
use fenestra_wire::{mint, SessionKey, Viewer};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-view: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--headless") {
        die("only --headless exists yet");
    }
    let dir = PathBuf::from(arg(&args, "--mint-dir").unwrap_or_else(|| die("--mint-dir DIR is needed")));
    let out = PathBuf::from(arg(&args, "--out").unwrap_or_else(|| die("--out DIR is needed")));
    let run_secs: u64 = arg(&args, "--run-secs").and_then(|s| s.parse().ok()).unwrap_or(20).min(3600);
    let reconnect: Option<u64> = arg(&args, "--reconnect-after").and_then(|s| s.parse().ok());
    // The host's address as this side reaches it (a forwarded port), in
    // place of the one in the mint.
    // Bytes received after this many seconds count as idle (`idle_bytes`).
    // Input to send, from a script (`--input FILE`): lines of
    // `SECONDS MOVE x y | BTN button 0|1 | KEY usage 0|1 [modifiers] | SCROLL dy`,
    // seconds from the start, in the host screen's pixels.
    let script = arg(&args, "--input");
    let idle_after: Option<u64> = arg(&args, "--idle-after").and_then(|s| s.parse().ok());
    let connect: Option<std::net::SocketAddr> = arg(&args, "--connect").map(|a| a.parse().unwrap_or_else(|_| die("--connect ADDR:PORT")));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    rt.block_on(async move {
        let key = SessionKey::generate().unwrap_or_else(|e| die(e));
        mint::write_viewer_key(&dir, key.public()).unwrap_or_else(|e| die(format!("{}: {e}", dir.display())));
        let mut tries = 0;
        let m = loop {
            match mint::DevMint::read(&dir) {
                Ok(m) => break m,
                Err(_) if tries < 600 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => die(format!("no mint: {e}")),
            }
        };
        let viewer = Viewer::new(&key, &m.host_key).unwrap_or_else(|e| die(e));
        let addr = connect.unwrap_or(m.addr);
        let views = Arc::new(Views::default());
        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<Input>(1024);
        let inputs: fenestra_view::Inputs = Arc::new(tokio::sync::Mutex::new(input_rx));
        if let Some(f) = &script {
            let lines = std::fs::read_to_string(f).unwrap_or_else(|e| die(format!("{f}: {e}")));
            let steps = parse_script(&lines).unwrap_or_else(|e| die(format!("{f}: {e}")));
            let start = tokio::time::Instant::now();
            tokio::spawn(async move {
                for (at, i) in steps {
                    tokio::time::sleep_until(start + Duration::from_secs_f64(at)).await;
                    if input_tx.send(i).await.is_err() {
                        return;
                    }
                }
            });
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(run_secs);
        let mark = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        if let Some(s) = idle_after {
            let (views, mark) = (views.clone(), mark.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(s)).await;
                mark.store(views.bytes.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
            });
        }
        let mut first = true;
        loop {
            let hc = viewer.connect(addr, &m.secret).await.unwrap_or_else(|e| die(e));
            eprintln!("fenestra-view: connected to {addr}");
            let cut = match (first, reconnect) {
                (true, Some(s)) => tokio::time::Instant::now() + Duration::from_secs(s),
                _ => deadline,
            };
            first = false;
            let conn = hc.conn.clone();
            tokio::select! {
                r = run(hc, views.clone(), inputs.clone()) => {
                    if let Err(e) = r {
                        die(e);
                    }
                    break;
                }
                _ = tokio::time::sleep_until(cut) => {
                    if cut >= deadline {
                        break;
                    }
                    eprintln!("fenestra-view: dropping the connection to reconnect");
                    conn.close(0u32.into(), b"reconnect");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        let mut report = snapshot(&views, &out).unwrap_or_else(|e| die(e));
        let at = mark.load(std::sync::atomic::Ordering::Relaxed);
        if at != u64::MAX {
            let line = format!("idle_bytes {}\n", views.bytes.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(at));
            report += &line;
            let _ = std::fs::OpenOptions::new().append(true).open(out.join("viewer.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        }
        print!("{report}");
        viewer.close();
    });
}

fn parse_script(text: &str) -> Result<Vec<(f64, Input)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: {line:?}", n + 1);
        let w: Vec<&str> = line.split_whitespace().collect();
        let f = |i: usize| w.get(i).and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite()).ok_or_else(bad);
        let on = |i: usize| match w.get(i) {
            Some(&"1") => Ok(true),
            Some(&"0") => Ok(false),
            _ => Err(bad()),
        };
        let at = w.first().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0).ok_or_else(bad)?;
        let i = match w.get(1).copied() {
            Some("MOVE") => Input::Motion { x: f(2)?, y: f(3)? },
            Some("BTN") => Input::Button { button: w.get(2).and_then(|v| v.parse().ok()).filter(|b| (1..=5).contains(b)).ok_or_else(bad)?, pressed: on(3)? },
            Some("KEY") => Input::Key {
                usage: w.get(2).and_then(|v| v.parse().ok()).ok_or_else(bad)?,
                pressed: on(3)?,
                modifiers: w.get(4).map_or(Some(0), |v| v.parse().ok()).ok_or_else(bad)?,
            },
            Some("SCROLL") => Input::Scroll { dx: 0.0, dy: f(2)? },
            _ => return Err(bad()),
        };
        out.push((at, i));
    }
    Ok(out)
}
