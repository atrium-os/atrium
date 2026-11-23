//! Synchronous fixture calls on a single-element private channel. LibWeb
//! invokes append from its MSE task, after copying the JavaScript buffer.
use navigator_control::video::{self, Reply, Request};
use std::{
    os::{fd::BorrowedFd, unix::net::UnixStream},
    sync::{Mutex, OnceLock},
    time::Duration,
};

struct Player {
    socket: UnixStream,
    buffers: [Option<Buffer>; 2],
    time: f64,
    playing: bool,
}

struct Buffer {
    id: u64,
    ranges: Vec<(f64, f64)>,
}

static PLAYER: OnceLock<Mutex<Player>> = OnceLock::new();

#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_video_channel(fd: i32) -> bool {
    let Ok(fetch) = (unsafe { BorrowedFd::borrow_raw(fd) }).try_clone_to_owned() else {
        return false;
    };
    let fetch = UnixStream::from(fetch);
    let timeout = Some(Duration::from_secs(8));
    if fetch.set_read_timeout(timeout).is_err() {
        return false;
    }
    let Ok(media) = navigator_control::recv_fd(&fetch) else {
        return false;
    };
    let socket = UnixStream::from(media);
    if socket
        .set_read_timeout(timeout)
        .and_then(|_| socket.set_write_timeout(timeout))
        .is_err()
    {
        return false;
    }
    PLAYER
        .set(Mutex::new(Player {
            socket,
            buffers: [None, None],
            time: 0.0,
            playing: false,
        }))
        .is_ok()
}

impl Player {
    fn call(&mut self, request: Request) -> Option<Reply> {
        video::send(&mut self.socket, &request).ok()?;
        video::receive(&mut self.socket).ok()
    }
}

fn player() -> Option<std::sync::MutexGuard<'static, Player>> {
    PLAYER.get()?.lock().ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_available() -> bool {
    PLAYER.get().is_some()
}

#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_open(x: u16, y: u16, w: u16, h: u16) -> bool {
    matches!(
        player().and_then(|mut p| p.call(Request::Open { rect: [x, y, w, h] })),
        Some(Reply::Ok)
    )
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_add(id: u64, opus: bool) -> bool {
    let Some(mut p) = player() else { return false };
    let slot = usize::from(opus);
    if p.buffers[slot].is_some() || p.buffers.iter().flatten().any(|b| b.id == id) {
        return false;
    }
    let req = if opus {
        Request::AddOpus
    } else {
        Request::AddVp9
    };
    if !matches!(p.call(req), Some(Reply::Ok)) {
        return false;
    }
    p.buffers[slot] = Some(Buffer {
        id,
        ranges: Vec::new(),
    });
    true
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_video_append(id: u64, ptr: *const u8, len: usize) -> bool {
    if ptr.is_null() || !(1..=video::MAX_APPEND).contains(&len) {
        return false;
    }
    let Some(mut p) = player() else { return false };
    let Some(slot) = p
        .buffers
        .iter()
        .position(|b| b.as_ref().is_some_and(|b| b.id == id))
    else {
        return false;
    };
    let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    let Some(Reply::Appended { ranges }) = p.call(if slot == 0 {
        Request::Append(data)
    } else {
        Request::AppendOpus(data)
    }) else {
        return false;
    };
    if ranges.len() > 64
        || !ranges.iter().all(|(a, b)| {
            limen::media_player::finite_secs(*a) && limen::media_player::finite_secs(*b) && a <= b
        })
    {
        return false;
    }
    p.buffers[slot].as_mut().unwrap().ranges = ranges;
    true
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_range(id: u64, i: usize, start: &mut f64, end: &mut f64) -> bool {
    let Some((a, b)) = player().and_then(|p| {
        p.buffers
            .iter()
            .flatten()
            .find(|b| b.id == id)?
            .ranges
            .get(i)
            .copied()
    }) else {
        return false;
    };
    *start = a;
    *end = b;
    true
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_end() -> f64 {
    match player().and_then(|mut p| p.call(Request::End)) {
        Some(Reply::Ready { duration }) if limen::media_player::finite_secs(duration) => duration,
        _ => -1.0,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_seek(seconds: f64) -> bool {
    let Some(mut p) = player() else { return false };
    let Some(Reply::Seeked { secs }) = p.call(Request::Seek(seconds)) else {
        return false;
    };
    if !limen::media_player::finite_secs(secs) {
        return false;
    }
    p.time = secs;
    true
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_playing(playing: bool) {
    if let Some(mut p) = player() {
        if matches!(p.call(Request::SetPlaying(playing)), Some(Reply::Ok)) {
            if !playing {
                if let Some(Reply::Position { secs }) = p.call(Request::Clock) {
                    if limen::media_player::finite_secs(secs) {
                        p.time = secs;
                    }
                }
            }
            p.playing = playing;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_volume(volume: f64) {
    if let Some(mut p) = player() {
        let _ = p.call(Request::Volume(volume));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_time() -> f64 {
    let Some(mut p) = player() else { return 0.0 };
    if p.playing {
        if let Some(Reply::Position { secs }) = p.call(Request::Clock) {
            if limen::media_player::finite_secs(secs) {
                p.time = secs;
            }
        }
    }
    p.time
}
