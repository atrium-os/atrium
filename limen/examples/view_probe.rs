//! A stand-in viewer for quick look's VM gate (scripts/vm-quick-look.sh,
//! opening-files.md §7): a Limen child for `doc-viewer` or `media-player`
//! that shows a plain colour in its slot and says, for each file it is
//! handed, what it got and what it can do with it. Started by limend
//! through portcullisd, like any provider.
use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use fresco_protocol::{RectParams, WindowHints};
use limen::proto::ChildEvent;

fn err() -> String {
    format!("E{}", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

fn main() {
    let (mut child, fresco) = match limen::client::Child::embedded() {
        Ok(v) => v,
        Err(e) => return eprintln!("view-probe: no limend channel: {e:?}"),
    };
    let (embed, role) = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, parent_app, .. })) => {
                eprintln!("view-probe: attached as {role} for {parent_app}, pid {}", std::process::id());
                break (embed, role);
            }
            Ok(Some(_)) => {}
            _ => return eprintln!("view-probe: never attached"),
        }
    };
    let mut c = match fresco_client::Connection::wrap(fresco) {
        Ok(c) => c,
        Err(e) => return eprintln!("view-probe: frescod: {e}"),
    };
    let hints = WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() };
    let win = match c.window_create(320, 240, "view-probe", hints) {
        Ok(w) => w,
        Err(e) => return eprintln!("view-probe: window: {e}"),
    };
    c.set_default_window(win as u16);
    let colour = if role == "media-player" { [0.15, 0.35, 0.75] } else { [0.2, 0.6, 0.35] };
    let paint = |c: &mut fresco_client::Connection| {
        let _ = c.scene_frame_begin();
        let _ = c.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 4000.0, h: 4000.0, r: colour[0], g: colour[1], b: colour[2], a: 1.0, radius: 0.0 });
        let _ = c.scene_frame_end();
    };
    paint(&mut c);
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        match c.wait_event(Some(Duration::from_millis(100))) {
            Ok(Some(fresco_client::Event::EmbedToken { token, .. })) => break token,
            Err(e) => return eprintln!("view-probe: frescod: {e}"),
            _ if Instant::now() > until => return eprintln!("view-probe: no embed token"),
            _ => {}
        }
    };
    if child.ready(embed, token).is_err() {
        return eprintln!("view-probe: ready refused");
    }
    loop {
        if let Ok(Some(fresco_client::Event::Resized { .. })) = c.wait_event(Some(Duration::from_millis(20))) {
            paint(&mut c);
        }
        match child.poll(Duration::from_millis(50)) {
            Ok(Some(ChildEvent::MessageFd { bytes, .. })) => {
                let what = if role == "media-player" {
                    match limen::media_player::ToChild::decode(&bytes) {
                        Some(limen::media_player::ToChild::Load { mime, .. }) => format!("load mime={mime}"),
                        other => format!("{other:?}"),
                    }
                } else {
                    match limen::doc_viewer::ToChild::decode(&bytes) {
                        Some(limen::doc_viewer::ToChild::Load { kind, .. }) => format!("load kind={kind:?}"),
                        other => format!("{other:?}"),
                    }
                };
                let Some(fd) = child.take_fd() else {
                    eprintln!("view-probe: {what} without a file");
                    continue;
                };
                let mut f = std::fs::File::from(fd);
                let fd = f.as_raw_fd();
                let mut head = [0u8; 16];
                let n = f.read(&mut head).unwrap_or(0);
                let head: String = head[..n].iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '.' }).collect();
                // SAFETY: calls on a descriptor we own, with buffers we own.
                let wrote = unsafe { if libc::pwrite(fd, b"x".as_ptr().cast(), 1, 0) == 1 { "ok".to_string() } else { err() } };
                let chmod = unsafe { if libc::fchmod(fd, 0o777) == 0 { "ok".to_string() } else { err() } };
                let mmap = unsafe {
                    let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
                    if p == libc::MAP_FAILED { err() } else { libc::munmap(p, 4096); "ok".into() }
                };
                let kinfo = {
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
                eprintln!("view-probe: {role} {what} head={head} wrote={wrote} chmod={chmod} mmap={mmap} kinfo={kinfo} media={media} uid={}", unsafe { libc::getuid() });
            }
            Ok(Some(ChildEvent::Detaching { .. })) | Err(_) => return eprintln!("view-probe: {role} detached"),
            Ok(_) => {}
        }
    }
}
