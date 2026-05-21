//! atrium-paint — fourth sample Insula app and the
//! first to use the Pergola path.
//!
//! Lifecycle:
//!   1. atrium_init.
//!   2. atrium_window_open  — get a window_id from
//!      the Fresco scene server.
//!   3. atrium_window_fill_rect — paint the whole
//!      window magenta.
//!   4. Loop on atrium_window_poll_event until either
//!      a CLOSE_REQUESTED event arrives OR a deadline
//!      elapses (so a test harness can run us
//!      headlessly without a real X-button).
//!   5. atrium_window_destroy + atrium_exit.
//!
//! Exit code 0 = clean exit; 1 = couldn't reach
//! frescod (no $ATRIUM_FRESCO_SOCKET / connect refused);
//! 2 = window creation failed.

use atrium::{
    atrium_exit, atrium_init, atrium_log,
    atrium_window_destroy, atrium_window_fill_rect,
    atrium_window_open, atrium_window_poll_event,
    AtriumWindowEvent, ATRIUM_ERR_NO_FRESCO,
    ATRIUM_EV_WINDOW_CLOSE_REQUESTED,
    ATRIUM_LOG_ERROR, ATRIUM_LOG_INFO,
};
use std::ffi::CString;
use std::time::{Duration, Instant};

fn log_info(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_INFO, c.as_ptr()); }
}

fn log_error(msg: &str) {
    let c = CString::new(msg).unwrap();
    unsafe { atrium_log(ATRIUM_LOG_ERROR, c.as_ptr()); }
}

fn main() {
    let status = atrium_init(1, 0);
    if status != atrium::ATRIUM_OK {
        eprintln!("atrium_init failed: {}", status);
        std::process::exit(1);
    }

    // Test harnesses set ATRIUM_PAINT_MAX_MS to cap
    // how long we poll for events. Real use lets the
    // user click the X.
    let max_ms: u64 = std::env::var("ATRIUM_PAINT_MAX_MS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(60_000);

    let title = CString::new("atrium-paint").unwrap();
    let id = unsafe { atrium_window_open(title.as_ptr(), 640, 480) };
    if id == ATRIUM_ERR_NO_FRESCO {
        log_error("atrium-paint: $ATRIUM_FRESCO_SOCKET not set; \
                   no Fresco scene server reachable");
        atrium_exit(1);
    }
    if id < 0 {
        log_error(&format!(
            "atrium-paint: window_open failed: code {}", id
        ));
        atrium_exit(2);
    }
    log_info(&format!("atrium-paint: opened window id={}", id));
    let window_id = id as u32;

    // Paint magenta over the whole canvas.
    let r = atrium_window_fill_rect(
        window_id,
        0.0, 0.0, 640.0, 480.0,
        1.0, 0.0, 1.0, 1.0,
    );
    if r != 0 {
        log_error(&format!(
            "atrium-paint: fill_rect failed: code {}", r
        ));
        // Still try to clean up the window.
    } else {
        log_info("atrium-paint: painted initial frame (magenta)");
    }

    // Event loop. Exit on CLOSE_REQUESTED or deadline.
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    let mut closed = false;
    while Instant::now() < deadline {
        let mut ev = AtriumWindowEvent {
            kind: 0, _pad: 0, window_id: 0, arg1: 0, arg2: 0,
        };
        let rc = unsafe { atrium_window_poll_event(&mut ev) };
        match rc {
            1 => {
                log_info(&format!(
                    "atrium-paint: event kind=0x{:04x} window={} arg1={} arg2={}",
                    ev.kind, ev.window_id, ev.arg1, ev.arg2,
                ));
                if ev.kind == ATRIUM_EV_WINDOW_CLOSE_REQUESTED
                    && ev.window_id == window_id
                {
                    closed = true;
                    break;
                }
            }
            0 => {
                std::thread::sleep(Duration::from_millis(16));
            }
            _ => {
                log_error(&format!("atrium-paint: poll failed: code {}", rc));
                break;
            }
        }
    }

    if closed {
        log_info("atrium-paint: close requested, tearing down");
    } else {
        log_info(&format!(
            "atrium-paint: poll deadline ({}ms) elapsed, tearing down",
            max_ms,
        ));
    }
    let _ = atrium_window_destroy(window_id);
    atrium_exit(0);
}
