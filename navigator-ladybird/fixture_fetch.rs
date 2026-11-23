//! The fixtures use one preconnected Nuntius channel and a fixed GET sequence.
//! Its response must be a bounded, contiguous body with the expected media type at the exact fixture URL.
use nuntius::{Client, Event, Open};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const URL: &str = "https://editor.example/editor.html";
const COUNTER: &str = "https://widget.example/counter.html";
const SIGN_IN: [&str; 4] = [
    "https://idp.example/sign-in.html",
    "https://idp.example/sign-in.html?step=authorize",
    "https://idp.example/sign-in.html?step=password",
    "https://idp.example/sign-in.html?step=callback",
];
const PROVIDER: &[&str] = &[navigator_control::YOUTUBE_ENTRY];
const VIDEO: [&str; 4] = [
    "https://widget.example/navigator-i4/local-player.html",
    "https://widget.example/navigator-i4/local-player.js",
    "https://widget.example/media/red-blue.webm",
    "https://widget.example/media/tone-opus.webm",
];
type Sequence = &'static [&'static str];
static CLIENT: OnceLock<Mutex<Option<(Client, usize, Option<Sequence>)>>> = OnceLock::new();

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
    Client::from_stream(socket).is_ok_and(|c| CLIENT.set(Mutex::new(Some((c, 0, None)))).is_ok())
}

/// Return the response type only while this engine has an active provider document.
/// # Safety
/// The caller provides a readable serialized URL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_youtube_asset(url: *const u8, len: usize) -> u8 {
    let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, len) }) else {return 0};
    if !CLIENT.get().and_then(|c| c.lock().ok()).is_some_and(|s| s.as_ref().is_some_and(|(_,_,seq)| *seq == Some(PROVIDER))) {return 0;}
    match navigator_control::youtube::asset_mime(url) {Some("text/javascript") => 1, Some("text/css") => 2, _ => 0}
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
    let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, len) }) else {
        return 0;
    };
    let asset = navigator_control::youtube::asset_mime(url);
    let limit = if asset.is_some() { navigator_control::youtube::ASSET_LIMIT } else if url == navigator_control::YOUTUBE_ENTRY { navigator_control::YOUTUBE_ENTRY_LIMIT } else { 65536 };
    if capacity > limit { return 0; }
    let Some(mut state) = CLIENT.get().and_then(|c| c.lock().ok()) else {
        return 0;
    };
    if state.as_ref().is_some_and(|(_,step,seq)| *seq == Some(PROVIDER) && *step > 0 && asset.is_none()) {return 0;}
    let Some((mut client, step, sequence)) = state.take() else {
        return 0;
    };
    let sequence: &[&str] = match sequence {
        Some(sequence) => sequence,
        None if url == URL => &[URL],
        None if url == COUNTER => &[COUNTER],
        None if url == navigator_control::YOUTUBE_ENTRY => PROVIDER,
        None if url == VIDEO[0] => &VIDEO,
        None if url == SIGN_IN[0] => &SIGN_IN,
        _ => return 0,
    };
    let provider_asset = sequence == PROVIDER && step > 0;
    if provider_asset {
        if asset.is_none() || step > navigator_control::youtube::ASSET_COUNT {return 0;}
    } else if sequence.get(step).copied() != Some(url) { return 0; }
    let expected_mime = if provider_asset { asset.unwrap() } else if url == VIDEO[1] { "text/javascript" }
        else if url == VIDEO[2] { "video/webm" } else if url == VIDEO[3] { "audio/webm" } else { "text/html" };
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
                && mime == expected_mime
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
                if sequence == PROVIDER || step + 1 < sequence.len() {
                    *state = Some((client, step + 1, Some(sequence)));
                }
                return used;
            }
            _ => break,
        }
    }
    let _ = client.cancel(request_id);
    0
}
