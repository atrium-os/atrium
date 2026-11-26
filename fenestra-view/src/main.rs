//! `fenestra-view --mint-dir DIR [--connect ADDR]`: a window showing the
//! host's desktop; keyboard, pointer and wheel go to the host.
//!
//! `fenestra-view --headless --mint-dir DIR --out DIR [--run-secs N]
//! [--reconnect-after S] [--connect ADDR] [--idle-after S] [--input FILE]`
//! (gates, §14.2): draw the host's windows for `--run-secs`, then write
//! `window-ID.png`, `desktop.png` and `viewer.txt` to `--out`.
//! `--reconnect-after S` drops the connection after S seconds and connects
//! again, resuming from what it holds.
//!
//! Either way it connects with the mint in `--mint-dir`
//! (`fenestra_wire::mint`; it leaves its public key there first).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fenestra_view::{run, snapshot, Views};
use fenestra_wire::scene::Input;
use fenestra_wire::{mint, SessionKey, Viewer, WireError};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-view: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    let dir = PathBuf::from(arg(&args, "--mint-dir").unwrap_or_else(|| die("--mint-dir DIR is needed")));
    let out: Option<PathBuf> = arg(&args, "--out").map(PathBuf::from);
    if headless && out.is_none() {
        die("--headless needs --out DIR");
    }
    let run_secs: u64 = arg(&args, "--run-secs").and_then(|s| s.parse().ok()).unwrap_or(if headless { 20 } else { 7 * 86400 }).min(30 * 86400);
    let reconnect: Option<u64> = arg(&args, "--reconnect-after").and_then(|s| s.parse().ok());
    // The host's address as this side reaches it (a forwarded port), in
    // place of the one in the mint.
    // Bytes received after this many seconds count as idle (`idle_bytes`).
    // Input to send, from a script (`--input FILE`): lines of
    // `SECONDS MOVE x y | BTN button 0|1 | KEY usage 0|1 [modifiers] | SCROLL dy`,
    // seconds from the start, in the host screen's pixels. A line ending in
    // `PROBE` also times the input to the next frame (`probe AT MS`).
    let script = arg(&args, "--input");
    let idle_after: Option<u64> = arg(&args, "--idle-after").and_then(|s| s.parse().ok());
    let connect: Option<std::net::SocketAddr> = arg(&args, "--connect").map(|a| a.parse().unwrap_or_else(|_| die("--connect ADDR:PORT")));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    let views = Arc::new(Views::default());
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<Input>(1024);
    let window_input = input_tx.clone();
    let views_w = views.clone();
    let title = format!("Fenestra: {}", dir.display());
    let session = move || rt.block_on(async move {
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
        let probes: Arc<std::sync::Mutex<Vec<(f64, Option<f64>)>>> = Arc::default();
        let inputs: fenestra_view::Inputs = Arc::new(tokio::sync::Mutex::new(input_rx));
        if let Some(f) = &script {
            let lines = std::fs::read_to_string(f).unwrap_or_else(|e| die(format!("{f}: {e}")));
            let steps = parse_script(&lines).unwrap_or_else(|e| die(format!("{f}: {e}")));
            let start = tokio::time::Instant::now();
            let (views, probes) = (views.clone(), probes.clone());
            tokio::spawn(async move {
                for (at, i, probe) in steps {
                    tokio::time::sleep_until(start + Duration::from_secs_f64(at)).await;
                    let f0 = views.frames.load(std::sync::atomic::Ordering::Relaxed);
                    let t = tokio::time::Instant::now();
                    if input_tx.send(i).await.is_err() {
                        return;
                    }
                    if probe {
                        // Input to the first frame that follows it.
                        let (views, probes) = (views.clone(), probes.clone());
                        tokio::spawn(async move {
                            let mut ms = None;
                            while t.elapsed() < PROBE_TIMEOUT {
                                if views.frames.load(std::sync::atomic::Ordering::Relaxed) > f0 {
                                    ms = Some(t.elapsed().as_secs_f64() * 1e3);
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(2)).await;
                            }
                            probes.lock().unwrap_or_else(|e| e.into_inner()).push((at, ms));
                        });
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
        let mut connections = 0u32;
        'session: loop {
            // A connection lost to the network (a blackout past the idle
            // timeout, a host gone a while) is tried again until the
            // deadline; a host that breaks the protocol is not.
            let hc = loop {
                if tokio::time::Instant::now() >= deadline {
                    break 'session;
                }
                match tokio::time::timeout(CONNECT_TIMEOUT, viewer.connect(addr, &m.secret)).await {
                    Ok(Ok(hc)) => break hc,
                    Ok(Err(e @ (WireError::Proof | WireError::Protocol(_) | WireError::Tls(_)))) => die(e),
                    Ok(Err(e)) => eprintln!("fenestra-view: connect: {e}; trying again"),
                    Err(_) => eprintln!("fenestra-view: connect timed out; trying again"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            };
            connections += 1;
            eprintln!("fenestra-view: connected to {addr}");
            let cut = match (first, reconnect) {
                (true, Some(s)) => tokio::time::Instant::now() + Duration::from_secs(s),
                _ => deadline,
            };
            first = false;
            let conn = hc.conn.clone();
            tokio::select! {
                r = run(hc, views.clone(), inputs.clone()) => match r {
                    Ok(()) => break,
                    Err(e @ (WireError::Quic(_) | WireError::Timeout | WireError::Io(_))) => {
                        eprintln!("fenestra-view: connection lost ({e}); connecting again");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => die(e),
                },
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
        let Some(out) = out else {
            viewer.close();
            return;
        };
        let mut report = snapshot(&views, &out).unwrap_or_else(|e| die(e));
        let mut extra = format!("connections {connections}\n");
        let mut ps = probes.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ps.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (at, ms) in ps {
            extra += &match ms {
                Some(ms) => format!("probe {at} {ms:.1}\n"),
                None => format!("probe {at} timeout\n"),
            };
        }
        report += &extra;
        let _ = std::fs::OpenOptions::new().append(true).open(out.join("viewer.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, extra.as_bytes()));
        let at = mark.load(std::sync::atomic::Ordering::Relaxed);
        if at != u64::MAX {
            let line = format!("idle_bytes {}\n", views.bytes.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(at));
            report += &line;
            let _ = std::fs::OpenOptions::new().append(true).open(out.join("viewer.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        }
        print!("{report}");
        viewer.close();
    });
    if headless {
        session();
        return;
    }
    #[cfg(feature = "window")]
    {
        // The session on its own thread, the window on this one.
        std::thread::spawn(move || {
            session();
            eprintln!("fenestra-view: the session ended");
        });
        if let Err(e) = fenestra_view::window::run(views_w, window_input, title) {
            die(e);
        }
        std::process::exit(0);
    }
    #[cfg(not(feature = "window"))]
    {
        let _ = (views_w, window_input, title);
        die("built without the window feature: only --headless");
    }
}

/// How long a probe waits for its frame.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

fn parse_script(text: &str) -> Result<Vec<(f64, Input, bool)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: {line:?}", n + 1);
        let mut w: Vec<&str> = line.split_whitespace().collect();
        let probe = w.last() == Some(&"PROBE");
        if probe {
            w.pop();
        }
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
        out.push((at, i, probe));
    }
    Ok(out)
}
