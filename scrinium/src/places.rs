//! Where Scrinium's places come from (scrinium.md §3.1).
//!
//! - **Sticks.** The file manager holds `removable-files`: dispensatord
//!   mounts every stick served for the session into its jail at
//!   `/media/<part>` (device-assignment.md §4.8), and tells it on the
//!   `removable-media/` socket which come and go. Without that socket (a
//!   development run) the media folder is looked at every two seconds.
//! - **Folders it was given**, from `SCRINIUM_PLACES`
//!   (`Label=/path;Other=/path`): the app's `filesystem` grants.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::browser::Place;
use crate::fs::size_text;

pub const MEDIA_SOCKET: &str = "/atrium/sockets/removable-media/removable-media.sock";

/// Where the sticks are in Scrinium's jail: `SCRINIUM_MEDIA`, else `/media`.
pub fn media_dir() -> PathBuf {
    std::env::var_os("SCRINIUM_MEDIA").map(PathBuf::from).unwrap_or_else(|| "/media".into())
}

pub fn media_socket() -> PathBuf {
    std::env::var_os("SCRINIUM_MEDIA_SOCKET").map(PathBuf::from).unwrap_or_else(|| MEDIA_SOCKET.into())
}

/// A stick as a place: named by its size, as the bar names it.
pub fn stick(v: &atrium_media::Volume, media: &Path) -> Place {
    // A network share (`share-<uid>`) is named by where it is.
    let network = v.part.starts_with("share-") && !v.label.is_empty();
    let label = if network {
        v.label.clone()
    } else if v.size_bytes > 0 {
        format!("{} stick", size_text(v.size_bytes))
    } else {
        "USB stick".into()
    };
    Place {
        id: v.part.clone(),
        label,
        root: media.join(&v.part),
        stick: true,
        read_only: v.read_only,
        size: v.size_bytes,
        free: v.free_bytes,
        network,
        offline: false,
    }
}

/// `SCRINIUM_PLACES`.
pub fn configured() -> Vec<Place> {
    let Ok(s) = std::env::var("SCRINIUM_PLACES") else { return Vec::new() };
    parse_places(&s)
}

pub fn parse_places(s: &str) -> Vec<Place> {
    s.split(';')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(l, p)| !l.trim().is_empty() && p.starts_with('/'))
        .enumerate()
        .map(|(i, (l, p))| Place { id: format!("place:{i}"), label: l.trim().into(), root: p.into(), stick: false, read_only: false, size: 0, free: 0, network: false, offline: false })
        .collect()
}

/// Two places with the same label get their partition after it.
pub fn distinct(mut v: Vec<Place>) -> Vec<Place> {
    let labels: Vec<String> = v.iter().map(|p| p.label.clone()).collect();
    for p in v.iter_mut() {
        if p.stick && labels.iter().filter(|l| **l == p.label).count() > 1 {
            p.label = format!("{} ({})", p.label, p.id);
        }
    }
    v
}

/// The stick list as it changes, as places, on `tx`; runs until `tx` is
/// dropped by the receiver.
pub fn watch(tx: mpsc::Sender<Vec<Place>>) {
    let media = media_dir();
    let sock = media_socket();
    std::thread::spawn(move || loop {
        match atrium_media::client::Events::open(&sock) {
            Ok(mut ev) => {
                let mut list: Vec<atrium_media::Volume> = Vec::new();
                // The Added events of what is served now come first.
                let _ = tx.send(Vec::new());
                loop {
                    match ev.next() {
                        Ok(atrium_media::Event::Added(v)) => {
                            list.retain(|o| o.part != v.part);
                            list.push(v);
                        }
                        Ok(atrium_media::Event::Removed { part, .. }) => list.retain(|o| o.part != part),
                        Err(_) => break,
                    }
                    list.sort_by(|a, b| a.part.cmp(&b.part));
                    if tx.send(list.iter().map(|v| stick(v, &media)).collect()).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                log::info!("scrinium: no media events ({e}); looking at {} instead", media.display());
                let mut last = None;
                loop {
                    let now = scan(&media);
                    if last.as_ref() != Some(&now) {
                        if tx.send(now.clone()).is_err() {
                            return;
                        }
                        last = Some(now);
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    if sock.exists() && atrium_media::client::Events::open(&sock).is_ok() {
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    });
}

/// The folders in `media` that are mounts of their own (a stick).
pub fn scan(media: &Path) -> Vec<Place> {
    use std::os::unix::fs::MetadataExt;
    let Ok(top) = std::fs::metadata(media) else { return Vec::new() };
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(media) else { return out };
    for e in rd.flatten() {
        let Ok(name) = e.file_name().into_string() else { continue };
        if name.starts_with('.') || !crate::fs::valid_name(&name) {
            continue;
        }
        let Ok(m) = std::fs::metadata(e.path()) else { continue };
        let mounted = m.dev() != top.dev();
        if !m.is_dir() || (!mounted && std::env::var_os("SCRINIUM_MEDIA_ANY").is_none()) {
            continue;
        }
        let (size, free, ro) = statvfs(&e.path()).unwrap_or((0, 0, false));
        out.push(stick(&atrium_media::Volume { part: name, mountpoint: String::new(), size_bytes: size, free_bytes: free, read_only: ro, label: String::new() }, media));
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// (size, free, read-only) of the file system at `p`.
pub fn statvfs(p: &Path) -> Option<(u64, u64, bool)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs into a zeroed struct we own, with a NUL-terminated path.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        let f = s.f_frsize as u64;
        Some((s.f_blocks as u64 * f, s.f_bavail as u64 * f, s.f_flag & libc::ST_RDONLY != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticks_and_configured_places() {
        let v = atrium_media::Volume { part: "da0s1".into(), mountpoint: "/media/da0s1".into(), size_bytes: 16_000_000_000, free_bytes: 1, read_only: false, label: String::new() };
        let p = stick(&v, Path::new("/media"));
        assert_eq!((p.label.as_str(), p.root.as_path(), p.stick), ("16 GB stick", Path::new("/media/da0s1"), true));
        let two = distinct(vec![p.clone(), Place { id: "da1s1".into(), ..p.clone() }]);
        assert_eq!(two[1].label, "16 GB stick (da1s1)");
        let c = parse_places("Documents=/home/ana/Documents; bad ;Rel=relative;=/x");
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].label.as_str(), c[0].stick), ("Documents", false));
    }
}
