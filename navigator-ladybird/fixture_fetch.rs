//! I0 fixtures use one preconnected Nuntius channel and a fixed GET sequence.
//! Its response must be a bounded, contiguous HTML body at the exact fixture URL.
use nuntius::{Client, Event, Open};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const URL: &str = "https://editor.example/editor.html";
const SIGN_IN: [&str; 4] = [
    "https://idp.example/sign-in.html",
    "https://idp.example/sign-in.html?step=authorize",
    "https://idp.example/sign-in.html?step=password",
    "https://idp.example/sign-in.html?step=callback",
];
static CLIENT: OnceLock<Mutex<Option<(Client, usize)>>> = OnceLock::new();

/// # Safety
/// The caller transfers sole ownership of a connected descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_fixture_channel(fd: i32) -> bool {
    use std::os::fd::FromRawFd;
    let socket = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    if socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return false;
    }
    Client::from_stream(socket).is_ok_and(|c| CLIENT.set(Mutex::new(Some((c, 0)))).is_ok())
}

/// # Safety
/// The URL and output buffer are valid for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_fixture_get(
    url: *const u8,
    len: usize,
    out: *mut u8,
    capacity: usize,
) -> usize {
    if capacity > 65536 {
        return 0;
    }
    let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, len) }) else {
        return 0;
    };
    let Some(mut state) = CLIENT.get().and_then(|c| c.lock().ok()) else {
        return 0;
    };
    let Some((mut client, step)) = state.take() else {
        return 0;
    };
    if !(step == 0 && url == URL) && SIGN_IN.get(step).copied() != Some(url) {
        return 0;
    }
    let request_id = step as u32 + 1;
    let mut open = Open::get(request_id, url);
    open.limits.max_bytes = capacity as u64;
    open.retry.max_attempts = 0;
    if client.open(open).is_err() {
        return 0;
    }
    let until = Instant::now() + Duration::from_secs(5);
    let mut total = None;
    let mut used = 0;
    let output = unsafe { std::slice::from_raw_parts_mut(out, capacity) };
    while Instant::now() < until {
        match client.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Some(Event::Head {
                id,
                status: 200,
                final_url,
                redirects: 0,
                content_type: Some(mime),
                length: Some(n),
                ..
            })) if id == request_id
                && total.is_none()
                && final_url == url
                && mime == "text/html"
                && n <= capacity as u64 =>
            {
                total = Some(n as usize)
            }
            Ok(Some(Event::Data { id, offset, bytes }))
                if id == request_id
                    && offset == used as u64
                    && total.is_some_and(|n| bytes.len() <= n - used) =>
            {
                output[used..used + bytes.len()].copy_from_slice(&bytes);
                used += bytes.len();
            }
            Ok(Some(Event::Done { id, bytes }))
                if id == request_id && total == Some(used) && bytes == used as u64 =>
            {
                if url != URL && step + 1 < SIGN_IN.len() {
                    *state = Some((client, step + 1));
                }
                return used;
            }
            _ => break,
        }
    }
    let _ = client.cancel(request_id);
    0
}
