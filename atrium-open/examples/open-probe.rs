//! A test app for the opening-files gate (scripts/vm-open-files.sh): takes
//! files (and links, `link url=…`) from its document channel and says, for each, what it got and
//! what it can do with it. With each new file it reads the earlier ones
//! again (`again name=… read=ok|E5`), so the gate sees a disconnected
//! share or an ejected stick end them. Exits when the channel closes or after five
//! idle minutes.
use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::Duration;

fn err() -> String {
    let e = std::io::Error::last_os_error();
    format!("E{}", e.raw_os_error().unwrap_or(0))
}

fn main() {
    let mut ch = match atrium_open::Channel::connect() {
        Ok(Some(c)) => c,
        Ok(None) => {
            println!("open-probe: no document channel");
            return;
        }
        Err(e) => {
            println!("open-probe: channel: {e}");
            return;
        }
    };
    println!("open-probe: up, pid {}", std::process::id());
    let mut kept: Vec<(String, std::fs::File)> = Vec::new();
    loop {
        let mut o = match ch.recv_any(Some(Duration::from_secs(300))) {
            Ok(Some(atrium_open::Message::File(o))) => o,
            // A link (opening-links.md §5), as it came.
            Ok(Some(atrium_open::Message::Link(u))) => {
                println!("open-probe: link url={u}");
                continue;
            }
            Ok(Some(atrium_open::Message::Crossed { url, from })) => {
                println!("open-probe: link url={url} from={from}");
                continue;
            }
            Ok(None) => return println!("open-probe: idle, bye"),
            Err(e) => return println!("open-probe: channel: {e}"),
        };
        let fd = o.file.as_raw_fd();
        let mut head = [0u8; 16];
        let n = o.file.read(&mut head).unwrap_or(0);
        let first = head[0];
        let head: String = head[..n].iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '.' }).collect();
        // Write the first byte back where it was: harmless, and only
        // possible on a descriptor narrowed for an edit.
        // SAFETY: a one-byte buffer we own; fd is ours.
        let wrote = unsafe { if libc::pwrite(fd, b"x".as_ptr().cast(), 0, 0) == 0 && n > 0 && libc::pwrite(fd, (&first as *const u8).cast(), 1, 0) == 1 { "ok".into() } else { err() } };
        let chmod = unsafe { if libc::fchmod(fd, 0o777) == 0 { "ok".into() } else { err() } };
        let mmap = unsafe {
            let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
            if p == libc::MAP_FAILED { err() } else { libc::munmap(p, 4096); "ok".into() }
        };
        // F_KINFO (FreeBSD 13.1+): would it tell the app the file's path?
        let path = {
            #[cfg(target_os = "freebsd")]
            {
                let mut buf = vec![0u8; 1392];
                buf[..4].copy_from_slice(&1392i32.to_ne_bytes());
                // SAFETY: a kinfo_file-sized buffer with kf_structsize set.
                if unsafe { libc::fcntl(fd, 22, buf.as_mut_ptr()) } == 0 {
                    let p = &buf[368..];
                    let end = p.iter().position(|&b| b == 0).unwrap_or(p.len());
                    format!("[{}]", String::from_utf8_lossy(&p[..end]))
                } else {
                    err()
                }
            }
            #[cfg(not(target_os = "freebsd"))]
            {
                "-".to_string()
            }
        };
        let media = if std::path::Path::new("/media").exists() { "present" } else { "absent" };
        println!(
            "open-probe: got name={} type={} write={} head={head} wrote={wrote} chmod={chmod} mmap={mmap} kinfo={path} media={media}",
            o.name, o.mime, o.write as u8
        );
        for (name, f) in &kept {
            let mut b = [0u8; 1];
            // SAFETY: a one-byte buffer we own; the fd is ours.
            let r = unsafe { libc::pread(f.as_raw_fd(), b.as_mut_ptr().cast(), 1, 0) };
            println!("open-probe: again name={name} read={}", if r == 1 { "ok".to_string() } else { err() });
        }
        kept.push((o.name, o.file));
    }
}
