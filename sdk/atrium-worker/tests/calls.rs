use atrium_worker::{Error, Message, Workers};
use std::io::{Read, Seek, Write};
use std::time::Duration;

#[derive(Message, Debug, PartialEq)]
#[message(version = 1)]
struct Ask {
    #[message(max_len = 64)]
    what: String,
}

#[derive(Message, Debug, PartialEq)]
#[message(version = 1)]
struct Answer {
    #[message(max_len = 4096)]
    text: String,
    len: u64,
}

fn handle(a: Ask, f: Option<std::fs::File>) -> Result<Answer, String> {
    match a.what.as_str() {
        "read" => {
            let mut s = String::new();
            f.ok_or("no file")?.read_to_string(&mut s).map_err(|e| e.to_string())?;
            Ok(Answer { len: s.len() as u64, text: s })
        }
        "fail" => Err("asked to fail".into()),
        "crash" => std::process::exit(3),
        "abort" => std::process::abort(),
        "sleep" => { std::thread::sleep(Duration::from_secs(30)); Ok(Answer { text: String::new(), len: 0 }) }
        w => Ok(Answer { text: w.to_uppercase(), len: w.len() as u64 }),
    }
}

fn file(text: &str) -> std::fs::File {
    let mut f = tempfile();
    f.write_all(text.as_bytes()).unwrap();
    f.rewind().unwrap();
    f
}

fn tempfile() -> std::fs::File {
    let p = std::env::temp_dir().join(format!("atrium-worker-{}-{:?}", std::process::id(), std::thread::current().id()));
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
    std::fs::remove_file(&p).unwrap();
    f
}

/// The child side of `local_children`: this test binary, re-run.
#[test]
fn worker_child() {
    if std::env::var_os("ATRIUM_WORKER_TEST_CHILD").is_none() { return }
    atrium_worker::serve(handle)
}

#[test]
fn in_process() {
    let w = Workers::in_process().register("up", Duration::from_secs(5), handle);
    let a: Answer = w.call("up", &Ask { what: "hello".into() }, None).unwrap();
    assert_eq!(a, Answer { text: "HELLO".into(), len: 5 });
    let a: Answer = w.call("up", &Ask { what: "read".into() }, Some(file("from a file"))).unwrap();
    assert_eq!(a.text, "from a file");
    match w.call::<Ask, Answer>("up", &Ask { what: "fail".into() }, None) {
        Err(Error::Worker(m)) => assert_eq!(m, "asked to fail"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(w.call::<Ask, Answer>("nope", &Ask { what: "x".into() }, None), Err(Error::NoSuchWorker(_))));
    // A call over its bounds is refused by the worker's decoder.
    match w.call::<Ask, Answer>("up", &Ask { what: "x".repeat(65) }, None) {
        Err(Error::Worker(m)) => assert!(m.starts_with("call:"), "{m}"),
        r => panic!("{r:?}"),
    }
}

#[test]
fn local_children() {
    let dir = std::env::temp_dir().join(format!("atrium-worker-root-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let sh = format!("#!/bin/sh\nATRIUM_WORKER_TEST_CHILD=1 exec {} --exact worker_child --nocapture --test-threads 1\n", exe.display());
    std::fs::write(dir.join("bin/up"), sh).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("bin/up"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("atrium.toml"), "[app]\nid = \"org.example.t\"\nname = \"T\"\nversion = \"1\"\nentry = \"bin/core\"\n\n\
        [[workers]]\nname = \"up\"\nentry = \"bin/up\"\ncpu = \"2s\"\nlifetime = \"session\"\n").unwrap();
    let w = Workers::from_root(&dir, true).unwrap();
    assert_eq!(w.defs().next().unwrap().timeout(), Duration::from_secs(2));

    let mut s = w.session("up").unwrap();
    let a: Answer = s.call(&Ask { what: "one".into() }, None).unwrap();
    assert_eq!(a.text, "ONE");
    let a: Answer = s.call(&Ask { what: "read".into() }, Some(file("two"))).unwrap();
    assert_eq!(a.text, "two");
    drop(s);

    match w.call::<Ask, Answer>("up", &Ask { what: "crash".into() }, None) {
        Err(Error::Crashed(Some(3))) => {}
        r => panic!("{r:?}"),
    }
    match w.call::<Ask, Answer>("up", &Ask { what: "abort".into() }, None) {
        Err(Error::Crashed(None)) => {}
        r => panic!("{r:?}"),
    }
    let t = std::time::Instant::now();
    match w.call::<Ask, Answer>("up", &Ask { what: "sleep".into() }, None) {
        Err(Error::Timeout(d)) => assert_eq!(d, Duration::from_secs(2)),
        r => panic!("{r:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(10));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn stdio_worker() {
    let dir = std::env::temp_dir().join(format!("atrium-worker-stdio-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/conv"), "#!/bin/sh\nread kind\necho \"note $kind\" >&2\ntr a-z A-Z\nexit 4\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("bin/conv"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("atrium.toml"), "[[workers]]\nname = \"conv\"\nentry = \"bin/conv\"\n").unwrap();
    let w = Workers::from_root(&dir, true).unwrap();
    assert!(w.has("conv"));
    let mut p = w.spawn_stdio("conv").unwrap();
    let mut i = p.stdin.take().unwrap();
    i.write_all(b"docx\nhello\n").unwrap();
    drop(i);
    let mut out = String::new();
    p.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let mut err = String::new();
    p.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert_eq!((out.as_str(), err.as_str()), ("HELLO\n", "note docx\n"));
    assert_eq!(p.wait(), Some(4));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The child side of `socket_worker_with_extra`: this test binary, re-run.
#[test]
fn socket_child() {
    if std::env::var_os("ATRIUM_WORKER_SOCKET_CHILD").is_none() { return }
    let mut st = atrium_worker::socket_worker(1).unwrap();
    let mut extra = String::new();
    std::fs::File::from(st.extra.remove(0)).read_to_string(&mut extra).unwrap();
    let mut line = [0u8; 5];
    st.sock.read_exact(&mut line).unwrap();
    write!(st.sock, "{} {} {}", String::from_utf8_lossy(&line), extra.trim(), st.capsicum).unwrap();
    std::process::exit(5);
}

#[test]
fn socket_worker_with_extra() {
    let dir = std::env::temp_dir().join(format!("atrium-worker-socket-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let sh = format!("#!/bin/sh\nATRIUM_WORKER_SOCKET_CHILD=1 exec {} --exact socket_child --nocapture --test-threads 1 >/dev/null\n", exe.display());
    std::fs::write(dir.join("bin/dec"), sh).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("bin/dec"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("atrium.toml"), "[[workers]]\nname = \"dec\"\nentry = \"bin/dec\"\nisolation = \"capsicum\"\n").unwrap();
    let w = Workers::from_root(&dir, true).unwrap();
    // The extra descriptor: a pipe with a line in it (another service's socket, in the Player).
    let (r, mut wr) = std::io::pipe().unwrap();
    wr.write_all(b"codec\n").unwrap();
    drop(wr);
    use std::os::fd::AsFd;
    let mut p = w.spawn_socket("dec", &[r.as_fd()]).unwrap();
    drop(r);
    p.sock.write_all(b"frame").unwrap();
    p.sock.shutdown(std::net::Shutdown::Write).unwrap();
    let mut out = String::new();
    p.sock.read_to_string(&mut out).unwrap();
    // Local mode: no capability mode asked for, whatever the manifest says.
    assert_eq!(out, "frame codec false");
    assert_eq!(p.wait(), Some(5));
    std::fs::remove_dir_all(&dir).unwrap();
}
