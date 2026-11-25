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
    match navigator_control::youtube::asset_mime(url) {Some("text/javascript") => 1, Some("text/css") => 2, Some("image/svg+xml") => 3, _ => 0}
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
    unsafe { fixture_request(url, len, out, capacity, None) }
}

/// # Safety
/// The caller supplies readable URL and body bytes and writable reply storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_player_post(
    url: *const u8,
    len: usize,
    body: *const u8,
    size: usize,
    out: *mut u8,
    capacity: usize,
) -> usize {
    if size == 0 || size > navigator_control::youtube::PLAYER_BODY_LIMIT {
        return 0;
    }
    let body = unsafe { std::slice::from_raw_parts(body, size) };
    unsafe { fixture_request(url, len, out, capacity, Some(body)) }
}

unsafe fn fixture_request(
    url: *const u8,
    len: usize,
    out: *mut u8,
    capacity: usize,
    post: Option<&[u8]>,
) -> usize {
    let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, len) }) else {
        return 0;
    };
    let media = post.is_some() && navigator_control::youtube_media::endpoint(url).is_some();
    if media && post.is_some_and(|data| data.len() > navigator_control::youtube_media::BODY) {
        return 0;
    }
    if post.is_some() && !media && !navigator_control::youtube::player_url(url) {
        return 0;
    }
    let asset = if media {
        Some(navigator_control::youtube_media::MIME)
    } else if post.is_some() {
        Some("application/json")
    } else {
        navigator_control::youtube::asset_mime(url)
    };
    let limit = if media {
        navigator_control::youtube_media::REPLY
    } else if post.is_some() {
        navigator_control::youtube::PLAYER_RESPONSE_LIMIT
    } else if asset.is_some() {
        navigator_control::youtube::asset_limit(url)
    } else if url == navigator_control::YOUTUBE_ENTRY {
        navigator_control::YOUTUBE_ENTRY_LIMIT
    } else {
        65536
    };
    if capacity > limit { return 0; }
    let Some(mut state) = CLIENT.get().and_then(|c| c.lock().ok()) else {
        return 0;
    };
    if state.as_ref().is_some_and(|(_,step,seq)| *seq == Some(PROVIDER) && *step > 0 && asset.is_none()) {return 0;}
    if post.is_some()
        && !state
            .as_ref()
            .is_some_and(|(_, step, seq)| *seq == Some(PROVIDER) && *step > 0)
    {
        return 0;
    }
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
        if asset.is_none() || step > navigator_control::youtube::ASSET_COUNT + navigator_control::youtube_media::MAX_REQUESTS {return 0;}
    } else if sequence.get(step).copied() != Some(url) { return 0; }
    let expected_mime = if provider_asset { asset.unwrap() } else if url == VIDEO[1] { "text/javascript" }
        else if url == VIDEO[2] { "video/webm" } else if url == VIDEO[3] { "audio/webm" } else { "text/html" };
    let request_id = step as u32 + 1;
    let mut open = match post {
        Some(bytes) => Open::post(request_id, url, if media { "application/octet-stream" } else { "application/json" }, bytes.to_vec()),
        None => Open::get(request_id, url),
    };
    open.limits.max_bytes = capacity as u64;
    open.retry.max_attempts = 0;
    if client.open(open).is_err() {
        return 0;
    }
    // A CDN response can outlast the local fixture timeout while the TLS
    // worker and broker are still delivering the bounded body.
    let until = Instant::now() + Duration::from_secs(if media { 15 } else { 5 });
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
            Ok(Some(Event::Error { id, .. })) if media && id == request_id => {
                *state = Some((client, step + 1, Some(sequence)));
                return 0;
            }
            _ => break,
        }
    }
    let _ = client.cancel(request_id);
    0
}

/// A whole page live (navigator-islands.md I6.1). The engine asks for its
/// page by `limen::web_island::LIVE_PAGE` and learns the address from the
/// answer's final URL; then every resource the page loads is asked of
/// navigatord by its own address, which serves it only inside the page's
/// site. Requests are numbered on from the page's; one at a time.
static LIVE_NEXT: Mutex<u32> = Mutex::new(1);

/// One whole GET on the channel: (status, final URL, content type, length)
/// with the body in `out`. `None` on a refusal, a failure, or anything out
/// of shape (a body over `out`, data out of order, a second head).
fn live_exchange(url: &str, out: &mut [u8], wait: Duration) -> Option<(u16, String, Option<String>, usize)> {
    live_exchange_with(url, None, out, wait)
}

/// As [`live_exchange`], or a script's POST of `post` (content type, body; I6.3).
fn live_exchange_with(url: &str, post: Option<(&str, &[u8])>, out: &mut [u8], wait: Duration) -> Option<(u16, String, Option<String>, usize)> {
    let mut state = CLIENT.get()?.lock().ok()?;
    let (client, _, _) = state.as_mut()?;
    let id = {
        let mut next = LIVE_NEXT.lock().ok()?;
        let id = *next;
        *next = next.checked_add(1)?;
        id
    };
    let mut open = match post {
        Some((kind, body)) => Open::post(id, url, kind, body.to_vec()),
        None => Open::get(id, url),
    };
    open.limits.max_bytes = out.len() as u64;
    open.retry.max_attempts = 0;
    client.open(open).ok()?;
    let until = Instant::now() + wait;
    let mut head = None;
    let mut used = 0usize;
    while Instant::now() < until {
        match client.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Some(Event::Head { id: got, status, final_url, content_type, length: Some(n), .. }))
                if got == id && head.is_none() && n <= out.len() as u64 =>
            {
                head = Some((status, final_url, content_type, n as usize));
            }
            Ok(Some(Event::Data { id: got, offset, bytes }))
                if got == id && offset == used as u64 && head.as_ref().is_some_and(|h| bytes.len() <= h.3 - used) =>
            {
                out[used..used + bytes.len()].copy_from_slice(&bytes);
                used += bytes.len();
            }
            Ok(Some(Event::Done { id: got, bytes })) if got == id => {
                let (status, final_url, kind, n) = head?;
                return (n == used && bytes == used as u64).then_some((status, final_url, kind, used));
            }
            Ok(Some(Event::Error { id: got, .. })) if got == id => return None,
            Ok(Some(_)) => continue,
            Ok(None) => continue,
            Err(_) => break,
        }
    }
    let _ = client.cancel(id);
    None
}

/// The page itself: its body in `out`, its address in `url_out`. Returns
/// the body's length, 0 when navigatord gave no HTML page.
/// # Safety
/// Both output buffers are writable for their lengths for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_live_page(url_out: *mut u8, url_cap: usize, url_len: *mut usize, out: *mut u8, cap: usize) -> usize {
    if url_out.is_null() || url_len.is_null() || out.is_null() || cap == 0 { return 0; }
    let body = unsafe { std::slice::from_raw_parts_mut(out, cap) };
    let Some((200, url, Some(kind), n)) = live_exchange(limen::web_island::LIVE_PAGE, body, Duration::from_secs(30)) else { return 0 };
    if kind != "text/html" || !url.starts_with("https://") || url.len() > url_cap { return 0; }
    unsafe {
        std::ptr::copy_nonoverlapping(url.as_ptr(), url_out, url.len());
        *url_len = url.len();
    }
    n
}

/// One resource of the live page, any type: its length (the body in
/// `out`), with the status and content type (`kind`, not terminated, its
/// length in `kind_len`, 0 for none). -1: refused or failed.
/// # Safety
/// The URL is readable and the outputs writable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_live_get(
    url: *const u8, len: usize, out: *mut u8, cap: usize,
    status: *mut u16, kind: *mut u8, kind_cap: usize, kind_len: *mut usize,
) -> isize {
    if url.is_null() || out.is_null() || status.is_null() || kind.is_null() || kind_len.is_null() || len > 8192 { return -1; }
    let Ok(url) = std::str::from_utf8(unsafe { std::slice::from_raw_parts(url, len) }) else { return -1 };
    let body = unsafe { std::slice::from_raw_parts_mut(out, cap) };
    let Some((code, final_url, content_type, n)) = live_exchange(url, body, Duration::from_secs(30)) else { return -1 };
    // navigatord follows no redirect for a resource: the answer is for this address.
    if final_url != url { return -1; }
    let content_type = content_type.unwrap_or_default();
    if content_type.len() > kind_cap { return -1; }
    unsafe {
        *status = code;
        std::ptr::copy_nonoverlapping(content_type.as_ptr(), kind, content_type.len());
        *kind_len = content_type.len();
    }
    n as isize
}

/// A script's POST (I6.3): `body` as `content_type` to `url`, which
/// navigatord sends through the page's jar inside its site. The answer as
/// for [`navigator_live_get`]; -1: refused or failed.
/// # Safety
/// The inputs are readable and the outputs writable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_live_post(
    url: *const u8, len: usize, content_type: *const u8, content_type_len: usize, body: *const u8, body_len: usize,
    out: *mut u8, cap: usize, status: *mut u16, kind: *mut u8, kind_cap: usize, kind_len: *mut usize,
) -> isize {
    if url.is_null() || content_type.is_null() || out.is_null() || status.is_null() || kind.is_null() || kind_len.is_null()
        || len > 8192 || content_type_len == 0 || content_type_len > 128 || body_len > nuntius::BODY_MAX || (body.is_null() && body_len > 0) {
        return -1;
    }
    // SAFETY: the caller passes readable buffers of these lengths.
    let (url, content_type, body) = unsafe { (
        std::slice::from_raw_parts(url, len), std::slice::from_raw_parts(content_type, content_type_len),
        if body_len == 0 { &[][..] } else { std::slice::from_raw_parts(body, body_len) },
    ) };
    let (Ok(url), Ok(content_type)) = (std::str::from_utf8(url), std::str::from_utf8(content_type)) else { return -1 };
    // SAFETY: the caller passes a writable buffer of `cap` bytes.
    let answer = unsafe { std::slice::from_raw_parts_mut(out, cap) };
    let Some((code, final_url, answer_type, n)) = live_exchange_with(url, Some((content_type, body)), answer, Duration::from_secs(30)) else { return -1 };
    if final_url != url { return -1; }
    let answer_type = answer_type.unwrap_or_default();
    if answer_type.len() > kind_cap { return -1; }
    // SAFETY: `status`, `kind` (`kind_cap` bytes) and `kind_len` are writable.
    unsafe {
        *status = code;
        std::ptr::copy_nonoverlapping(answer_type.as_ptr(), kind, answer_type.len());
        *kind_len = answer_type.len();
    }
    n as isize
}

/// A whole live page's `localStorage` (I6.3): navigatord keeps it for the
/// page's site; the engine loads it once and sends each change. The batch
/// format is navigatord's `live_storage`: per op a byte (1 set, 2 remove,
/// 3 clear, 4 append), then each string as a little-endian u32 length and
/// its UTF-8 bytes. A value too long for one batch goes as a set and appends.
mod live_storage {
    pub const SET: u8 = 1;
    pub const REMOVE: u8 = 2;
    pub const CLEAR: u8 = 3;
    pub const APPEND: u8 = 4;
    /// Room for one batch's strings under Nuntius's POST bound.
    pub const ROOM: usize = nuntius::BODY_MAX - 64;

    pub fn put(out: &mut Vec<u8>, s: &[u8]) {
        out.extend((s.len() as u32).to_le_bytes());
        out.extend(s);
    }

    /// Cut `s` into pieces of at most `room` bytes, each whole UTF-8.
    pub fn pieces(s: &str, room: usize) -> Vec<&str> {
        let mut out = vec![];
        let mut rest = s;
        while !rest.is_empty() {
            let mut at = rest.len().min(room);
            while !rest.is_char_boundary(at) { at -= 1; }
            let (a, b) = rest.split_at(at);
            out.push(a);
            rest = b;
        }
        if out.is_empty() { out.push(""); }
        out
    }
}

fn live_storage_send(batch: Vec<u8>) -> bool {
    let mut answer = [0u8; 16];
    let post = Some(("application/octet-stream", batch.as_slice()));
    matches!(live_exchange_with(limen::web_island::LIVE_STORAGE, post, &mut answer, Duration::from_secs(10)), Some((204, _, _, 0)))
}

/// Load the page's store: `on_item(ctx, key, key_len, value, value_len)`
/// for each item. False when navigatord gave none or it is out of shape.
/// # Safety
/// `on_item` is a valid function for this call; the strings it is given
/// live only for that call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_live_storage_load(
    ctx: *mut std::ffi::c_void, on_item: unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, usize, *const u8, usize),
) -> bool {
    // navigatord's bound: a 5 MiB quota and 8 bytes for each of at most
    // 65,536 items.
    let mut store = vec![0u8; 5 * 1024 * 1024 + 8 * 65_536];
    let Some((200, _, _, n)) = live_exchange(limen::web_island::LIVE_STORAGE, &mut store, Duration::from_secs(10)) else { return false };
    let mut rest = &store[..n];
    let mut items = vec![];
    let take = |rest: &mut &[u8]| -> Option<String> {
        let (len, more) = rest.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        if len > more.len() { return None; }
        let (s, more) = more.split_at(len);
        *rest = more;
        String::from_utf8(s.to_vec()).ok()
    };
    while !rest.is_empty() {
        let (Some(k), Some(v)) = (take(&mut rest), take(&mut rest)) else { return false };
        items.push((k, v));
    }
    for (k, v) in &items {
        // SAFETY: the caller's function, with strings alive for this call.
        unsafe { on_item(ctx, k.as_ptr(), k.len(), v.as_ptr(), v.len()) };
    }
    true
}

/// `localStorage.setItem(key, value)`: sent to navigatord, which keeps it
/// or refuses (over the site's quota). False: not kept.
/// # Safety
/// Both UTF-8 buffers are readable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_live_storage_set(key: *const u8, key_len: usize, value: *const u8, value_len: usize) -> bool {
    use live_storage::*;
    if key.is_null() || (value.is_null() && value_len > 0) || key_len > ROOM / 2 { return false; }
    // SAFETY: the caller passes readable buffers of these lengths.
    let (k, v) = unsafe { (std::slice::from_raw_parts(key, key_len), if value_len == 0 { &[][..] } else { std::slice::from_raw_parts(value, value_len) }) };
    let (Ok(k), Ok(v)) = (std::str::from_utf8(k), std::str::from_utf8(v)) else { return false };
    for (i, piece) in pieces(v, ROOM - k.len()).into_iter().enumerate() {
        let mut batch = vec![if i == 0 { SET } else { APPEND }];
        put(&mut batch, k.as_bytes());
        put(&mut batch, piece.as_bytes());
        if !live_storage_send(batch) { return false; }
    }
    true
}

/// `localStorage.removeItem(key)`.
/// # Safety
/// The UTF-8 buffer is readable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn navigator_live_storage_remove(key: *const u8, key_len: usize) -> bool {
    use live_storage::*;
    if key.is_null() || key_len > ROOM { return false; }
    // SAFETY: the caller passes a readable buffer of `key_len` bytes.
    let k = unsafe { std::slice::from_raw_parts(key, key_len) };
    let mut batch = vec![REMOVE];
    put(&mut batch, k);
    live_storage_send(batch)
}

/// `localStorage.clear()`.
#[unsafe(no_mangle)]
pub extern "C" fn navigator_live_storage_clear() -> bool {
    live_storage_send(vec![live_storage::CLEAR])
}

#[cfg(test)]
mod live_storage_tests {
    #[test]
    fn a_long_value_is_cut_on_character_boundaries() {
        let v = "é".repeat(10);
        let p = super::live_storage::pieces(&v, 5);
        assert!(p.iter().all(|s| s.len() <= 5));
        assert_eq!(p.concat(), v);
        assert_eq!(super::live_storage::pieces("", 5), vec![""]);
    }
}
