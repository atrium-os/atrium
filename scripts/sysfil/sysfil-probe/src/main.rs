//! sysfil-probe: exercises what an ordinary Atrium Rust app does through std
//! (threads, channels, files, directories, unix sockets, time, env, stdio,
//! a child-free event loop on poll), so that running it in a sysfil log-mode
//! jail yields the "base" profile every Rust app class starts from
//! (docs/spec/jail-sysfil.md §5). It deliberately does nothing an app class
//! should not: no exec, no inet sockets, no ptrace or ktrace.
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

fn main() {
    let dir = std::env::temp_dir().join(format!("sysfil-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let f = dir.join("a.txt");
    std::fs::write(&f, b"hello").expect("write");
    let mut s = String::new();
    std::fs::File::open(&f).unwrap().read_to_string(&mut s).unwrap();
    let _ = std::fs::metadata(&f).unwrap().modified();
    std::fs::rename(&f, dir.join("b.txt")).unwrap();
    for e in std::fs::read_dir(&dir).unwrap() { let _ = e.unwrap().file_type(); }
    let _ = std::fs::canonicalize(&dir);

    // Threads, a mutex/condvar pair (umtx), a channel.
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = mpsc::channel();
    let p2 = pair.clone();
    let h = std::thread::Builder::new().name("worker".into()).spawn(move || {
        let (m, c) = &*p2;
        *m.lock().unwrap() = true;
        c.notify_one();
        tx.send(42u32).unwrap();
    }).unwrap();
    let (m, c) = &*pair;
    let mut g = m.lock().unwrap();
    while !*g { g = c.wait_timeout(g, Duration::from_millis(100)).unwrap().0; }
    drop(g);
    assert_eq!(rx.recv().unwrap(), 42);
    h.join().unwrap();
    let _ = std::thread::available_parallelism();

    // A unix socket round trip, nonblocking accept like an event loop.
    let sock = dir.join("s");
    let l = UnixListener::bind(&sock).unwrap();
    let mut c1 = UnixStream::connect(&sock).unwrap();
    let (mut c2, _) = l.accept().unwrap();
    c1.set_nonblocking(true).unwrap();
    c1.set_read_timeout(None).unwrap();
    c2.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    std::thread::sleep(Duration::from_millis(5));
    let _ = c1.read(&mut buf);
    let (a, b) = UnixStream::pair().unwrap();
    drop((a, b));

    let t = Instant::now();
    let _ = SystemTime::now();
    std::thread::sleep(Duration::from_millis(1));
    let _ = t.elapsed();
    let _: Vec<(String, String)> = std::env::vars().collect();
    let _ = std::env::current_dir();
    let mut rng = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut u| u.read_exact(&mut rng)).ok();
    std::fs::remove_dir_all(&dir).unwrap();
    println!("sysfil-probe ok");
}
