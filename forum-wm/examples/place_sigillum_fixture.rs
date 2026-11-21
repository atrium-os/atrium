//! Place the test sheet below the trusted strip when running without Forum.
use fresco_client::Connection;
use fresco_protocol::{focus_cause, WmDeclareLayoutPayload, WmSlot};
use std::{io, time::Duration};

fn main() -> io::Result<()> {
    let mut wm = Connection::connect("wm.sock")?;
    wm.set_read_timeout(Some(Duration::from_secs(3)))?;
    let windows = wm.wm_enumerate()?;
    let mut sheets = windows.iter().filter(|w| w.owner_uid == 65030);
    let sheet = sheets
        .next()
        .ok_or_else(|| io::Error::other("no Sigillum sheet"))?;
    if sheets.next().is_some() {
        return Err(io::Error::other("multiple Sigillum sheets"));
    }
    let slots = windows
        .iter()
        .map(|w| {
            let mut rect = w.rect;
            let layer = if w.surface_id == sheet.surface_id {
                rect.x = 360;
                rect.y = 240;
                0
            } else {
                1
            };
            WmSlot {
                surface_id: w.surface_id,
                rect,
                layer,
            }
        })
        .collect();
    wm.wm_declare_layout(&WmDeclareLayoutPayload {
        slots,
        focus: sheet.surface_id,
        focus_cause: focus_cause::PROGRAM,
    })?;
    wm.wm_enumerate()?;
    Ok(())
}
