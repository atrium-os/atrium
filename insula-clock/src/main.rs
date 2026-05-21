//! insula-clock — fifth sample Insula app.
//!
//! Windowed analog clock that composes multiple Pergola
//! scene-node primitives every frame:
//!
//!   - 1 RECT for the dial background
//!   - 12 RECTs for the tick marks
//!   - 2 PATHs (rotated quads) for the hour + minute hands
//!
//! Animates at ~16 ms / frame. Exits on
//! EV_WINDOW_CLOSE_REQUESTED or when ATRIUM_CLOCK_MAX_MS
//! elapses (test harness uses the latter).
//!
//! Built on libatrium's portable C ABI rather than
//! fresco-rs direct — the visual sibling of the
//! existing atrium-clock, demonstrating that the
//! Insula-platform abstraction can drive the same UI
//! shape through a much smaller dependency surface.

use atrium::{
    atrium_exit, atrium_init, atrium_log,
    atrium_window_destroy, atrium_window_frame_begin,
    atrium_window_frame_end, atrium_window_frame_path,
    atrium_window_frame_rect, atrium_window_open,
    atrium_window_poll_event, AtriumWindowEvent,
    ATRIUM_ERR_NO_FRESCO, ATRIUM_EV_WINDOW_CLOSE_REQUESTED,
    ATRIUM_LOG_ERROR, ATRIUM_LOG_INFO,
};
use std::ffi::CString;
use std::f32::consts::PI;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const W: f32 = 400.0;
const H: f32 = 400.0;
const CX: f32 = W * 0.5;
const CY: f32 = H * 0.5;

fn log_info(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_INFO, c.as_ptr()); }
}

fn log_error(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_ERROR, c.as_ptr()); }
}

/// Paint one frame of the clock at the given
/// (hour, minute, second). Coordinates baked from W/H
/// constants.
fn paint_frame(window_id: u32, h: u32, m: u32, s: u32) -> i32 {
    if atrium_window_frame_begin(window_id) != 0 {
        return -1;
    }

    let mut next_node: u32 = 1;
    let mut node = || {
        let id = next_node;
        next_node += 1;
        id
    };

    // Background dial — large grey rect.
    atrium_window_frame_rect(
        node(),
        0.0, 0.0, W, H,
        0.10, 0.10, 0.12, 1.0,
    );

    // 12 tick marks around the dial.
    let radius_inner = 160.0;
    let radius_outer = 175.0;
    for i in 0..12 {
        let theta = (i as f32) * PI / 6.0 - PI / 2.0;
        let x = CX + theta.cos() * (radius_inner + radius_outer) * 0.5;
        let y = CY + theta.sin() * (radius_inner + radius_outer) * 0.5;
        let len = (radius_outer - radius_inner) * 1.5;
        // Use a path so the tick is rotated rather than
        // approximated by a rotated rect.
        atrium_window_frame_path(
            node(),
            x, y, len, 4.0, theta,
            0.8, 0.8, 0.85, 1.0,
        );
    }

    // Hour hand: 0..12 maps to 0..2π. Adjust for
    // current minutes for a smooth sweep.
    let hour_angle = ((h % 12) as f32 + (m as f32) / 60.0) * (PI / 6.0) - PI / 2.0;
    atrium_window_frame_path(
        node(),
        CX + hour_angle.cos() * 50.0, CY + hour_angle.sin() * 50.0,
        100.0, 8.0, hour_angle,
        1.0, 1.0, 1.0, 1.0,
    );

    // Minute hand.
    let minute_angle = (m as f32 + (s as f32) / 60.0) * (PI / 30.0) - PI / 2.0;
    atrium_window_frame_path(
        node(),
        CX + minute_angle.cos() * 70.0, CY + minute_angle.sin() * 70.0,
        140.0, 4.0, minute_angle,
        0.95, 0.95, 0.95, 1.0,
    );

    atrium_window_frame_end()
}

fn now_hms() -> (u32, u32, u32) {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = dur.as_secs();
    let h = ((secs / 3600) % 24) as u32;
    let m = ((secs / 60)   % 60) as u32;
    let s = (secs % 60) as u32;
    (h, m, s)
}

fn main() {
    if atrium_init(1, 0) != atrium::ATRIUM_OK {
        eprintln!("atrium_init failed");
        std::process::exit(1);
    }

    let max_ms: u64 = std::env::var("ATRIUM_CLOCK_MAX_MS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(60_000);

    let title = CString::new("insula-clock").unwrap();
    let id = unsafe { atrium_window_open(title.as_ptr(), W as u32, H as u32) };
    if id == ATRIUM_ERR_NO_FRESCO {
        log_error("insula-clock: $ATRIUM_FRESCO_SOCKET not set");
        atrium_exit(1);
    }
    if id < 0 {
        log_error(&format!("insula-clock: window_open failed: {}", id));
        atrium_exit(2);
    }
    let window_id = id as u32;
    log_info(&format!("insula-clock: opened window id={}", window_id));

    let deadline = Instant::now() + Duration::from_millis(max_ms);
    let mut frames: u64 = 0;
    let mut closed = false;
    while Instant::now() < deadline {
        let (h, m, s) = now_hms();
        if paint_frame(window_id, h, m, s) != 0 {
            log_error("insula-clock: paint_frame failed");
            break;
        }
        frames += 1;

        // Drain any pending events.
        loop {
            let mut ev = AtriumWindowEvent {
                kind: 0, _pad: 0, window_id: 0, arg1: 0, arg2: 0,
            };
            let rc = unsafe { atrium_window_poll_event(&mut ev) };
            if rc != 1 { break; }
            if ev.kind == ATRIUM_EV_WINDOW_CLOSE_REQUESTED {
                closed = true;
                break;
            }
        }
        if closed { break; }

        std::thread::sleep(Duration::from_millis(16));
    }

    log_info(&format!(
        "insula-clock: drew {} frame(s), closed={}", frames, closed
    ));
    let _ = atrium_window_destroy(window_id);
    atrium_exit(0);
}
