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
    ranges: Vec<(f64, f64)>,
    time: f64,
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
            ranges: vec![],
            time: 0.0,
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
pub extern "C" fn navigator_video_open(x: u16, y: u16, w: u16, h: u16) -> bool {
    matches!(
        player().and_then(|mut p| p.call(Request::Open { rect: [x, y, w, h] })),
        Some(Reply::Ok)
    )
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_add() -> bool {
    matches!(
        player().and_then(|mut p| p.call(Request::AddVp9)),
        Some(Reply::Ok)
    )
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_video_append(ptr: *const u8, len: usize) -> bool {
    if ptr.is_null() || !(1..=video::MAX_APPEND).contains(&len) {
        return false;
    }
    let Some(mut p) = player() else { return false };
    let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    let Some(Reply::Appended { ranges }) = p.call(Request::Append(data)) else {
        return false;
    };
    if ranges.len() > 64
        || !ranges.iter().all(|(a, b)| {
            limen::media_player::finite_secs(*a) && limen::media_player::finite_secs(*b) && a <= b
        })
    {
        return false;
    }
    p.ranges = ranges;
    true
}
#[unsafe(no_mangle)]
pub extern "C" fn navigator_video_range(i: usize, start: &mut f64, end: &mut f64) -> bool {
    let Some((a, b)) = player().and_then(|p| p.ranges.get(i).copied()) else {
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
pub extern "C" fn navigator_video_time() -> f64 {
    player().map_or(0.0, |p| p.time)
}
