//! forum-bar — the statusbar's data model (`docs/spec/forum.md` §3).
//!
//! The bar is an ordinary graphics-only app: it asks the WM core for the session's
//! surfaces over forum-ctl and summarises them. The *summary* logic lives here (pure
//! + testable); the bin does the I/O and (eventually) the drawing.

use forum_ctl::WmSurfaceInfo;

/// A one-line session status: window count + the focused app. This is what the bar
/// renders into its reserved top-edge surface (printed today, drawn via Pergola
/// once the display path is up).
pub fn status_line(surfaces: &[WmSurfaceInfo], focus: u32) -> String {
    let n = surfaces.len();
    let focused = surfaces
        .iter()
        .find(|s| s.surface_id == focus)
        .map(|s| s.owner_app.as_str())
        .unwrap_or("—");
    format!("{n} window{} · focus: {focused}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use forum_ctl::WmSurfaceInfo;
    use fresco_protocol::{WmRect, WmRole};

    fn surf(id: u32, app: &str) -> WmSurfaceInfo {
        WmSurfaceInfo {
            surface_id: id,
            owner_app: app.into(),
            owner_uid: 0,
            role: WmRole::Document,
            rect: WmRect { x: 0, y: 0, w: 0, h: 0 },
        }
    }

    #[test]
    fn summarises_count_and_focus() {
        let s = [surf(1, "editor"), surf(2, "terminal")];
        assert_eq!(status_line(&s, 2), "2 windows · focus: terminal");
    }

    #[test]
    fn singular_and_no_focus() {
        assert_eq!(status_line(&[surf(1, "editor")], 1), "1 window · focus: editor");
        assert_eq!(status_line(&[], 0), "0 windows · focus: —");
        // focus on a surface that's gone → no crash, dash.
        assert_eq!(status_line(&[surf(1, "editor")], 9), "1 window · focus: —");
    }
}
