//! `fenestra-session --mint-dir DIR [--bind ADDR]`: one Fenestra session on
//! Atrium (fenestra.md §3.2). `fenestrad` starts it with the link to
//! frescod's broker on its stdin (a socket); it terminates QUIC for the
//! viewers that prove the session, asks for each window's differences over
//! the link and sends them on, with the layout. It parses no scene record
//! and exits when the link ends.
//!
//! The mint goes through `--mint-dir` (`fenestra_wire::mint`, the
//! development mint) until the SSH mint lands.

use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fenestra_session::link::LinkSource;
use fenestra_session::{serve, ViewerStats};
use fenestra_wire::{mint, Host, SessionKey};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-session: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(arg(&args, "--mint-dir").unwrap_or_else(|| die("--mint-dir DIR is needed")));
    let bind = arg(&args, "--bind").unwrap_or_else(|| "0.0.0.0:0".into());
    // SAFETY: fd 0 is ours for the life of the process; fenestrad made it a
    // socket, and nothing else in this process uses it.
    let link = unsafe { std::os::unix::net::UnixStream::from_raw_fd(0) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    rt.block_on(run(link, dir, bind));
}

async fn run(link: std::os::unix::net::UnixStream, dir: PathBuf, bind: String) {
    link.set_nonblocking(true).unwrap_or_else(|e| die(format!("link: {e}")));
    let link = tokio::net::UnixStream::from_std(link).unwrap_or_else(|e| die(format!("link: {e}")));
    let (r, w) = link.into_split();
    let (src, wake) = LinkSource::start(r, w);

    let mut waited = 0;
    let viewer_key = loop {
        match mint::read_viewer_key(&dir) {
            Ok(k) => break k,
            Err(_) if waited < 1200 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += 1;
            }
            Err(e) => die(format!("no viewer key in {}: {e}", dir.display())),
        }
    };
    let key = SessionKey::generate().unwrap_or_else(|e| die(e));
    let secret = mint::fresh_secret().unwrap_or_else(|e| die(e));
    let host = Host::bind(bind.parse().unwrap_or_else(|_| die("--bind address")), &key, &viewer_key, secret.clone()).unwrap_or_else(|e| die(e));
    let addr = host.local_addr().unwrap_or_else(|e| die(e));
    mint::DevMint { addr, host_key: key.public().to_vec(), secret }.write(&dir).unwrap_or_else(|e| die(e));
    eprintln!("fenestra-session: serving on {addr}");
    src.session(true).await.unwrap_or_else(|e| die(e));

    let host = Arc::new(host);
    let mut n = 0u64;
    let mut ended = wake.clone();
    loop {
        tokio::select! {
            a = host.accept() => match a {
                Ok(Some(hc)) => {
                    n += 1;
                    let (src, wake, conn) = (src.clone(), wake.clone(), n);
                    tokio::spawn(async move {
                        let stats = Arc::new(ViewerStats::default());
                        let r = serve(hc, src, wake, stats.clone()).await;
                        eprintln!(
                            "fenestra-session: viewer {conn} gone ({}): frames {} wholes {} bytes {} acks {} resumed {} resent {} inputs {} dropped {}",
                            r.err().map_or("closed".to_string(), |e| e.to_string()),
                            stats.frames.load(std::sync::atomic::Ordering::Relaxed),
                            stats.wholes.load(std::sync::atomic::Ordering::Relaxed),
                            stats.bytes.load(std::sync::atomic::Ordering::Relaxed),
                            stats.acks.load(std::sync::atomic::Ordering::Relaxed),
                            stats.resumed.load(std::sync::atomic::Ordering::Relaxed),
                            stats.resent.load(std::sync::atomic::Ordering::Relaxed),
                            stats.inputs.load(std::sync::atomic::Ordering::Relaxed),
                            stats.inputs_dropped.load(std::sync::atomic::Ordering::Relaxed),
                        );
                    });
                }
                Ok(None) => break,
                Err(e) => eprintln!("fenestra-session: refused a connection: {e}"),
            },
            r = ended.changed() => if r.is_err() {
                eprintln!("fenestra-session: the link ended");
                break;
            },
        }
    }
    host.close_and_wait().await;
}
