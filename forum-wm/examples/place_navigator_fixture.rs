//! Position the single Navigator fixture window below the trusted strip.
use fresco_client::Connection;
use fresco_protocol::{focus_cause, WmDeclareLayoutPayload, WmSlot};
use std::{io, time::Duration};

fn main() -> io::Result<()> {
    let mut wm = Connection::connect("wm.sock")?;
    wm.set_read_timeout(Some(Duration::from_secs(5)))?;
    let surfaces = wm.wm_enumerate()?;
    let [window] = surfaces.as_slice() else {
        return Err(io::Error::other("expected just the Navigator start window"));
    };
    let mut rect = window.rect;
    if (rect.x, rect.w, rect.h) != (0, 1120, 740) {
        return Err(io::Error::other(format!("unexpected fixture geometry {rect:?}")));
    }
    rect.y = 40;
    wm.wm_declare_layout(&WmDeclareLayoutPayload {
        slots: vec![WmSlot { surface_id: window.surface_id, rect, layer: 0 }],
        focus: window.surface_id,
        focus_cause: focus_cause::PROGRAM,
    })?;
    wm.wm_enumerate()?;
    Ok(())
}
