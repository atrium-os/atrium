//! Place the test sheet below the trusted strip when running without Forum.
use fresco_client::Connection;
use fresco_protocol::{focus_cause, WmDeclareLayoutPayload, WmSlot};
use std::{io, time::Duration};

fn main() -> io::Result<()> {
    let mut wm = Connection::connect("wm.sock")?;
    wm.set_read_timeout(Some(Duration::from_secs(3)))?;
    let layout = wm.wm_enumerate_all()?;
    for surface in &layout.surfaces {
        println!(
            "surface {} uid={} role={:?} rect={:?}",
            surface.surface_id, surface.owner_uid, surface.role, surface.rect
        );
    }
    println!("embeds {:?}", layout.embeds);
    // `--focus UID`: give the focus back to that uid's window (a press
    // after the sheet closed is otherwise spent in a window without focus),
    // keeping every rectangle where it is.
    if std::env::args().nth(1).as_deref() == Some("--focus") {
        let uid: u32 = std::env::args().nth(2).and_then(|u| u.parse().ok()).ok_or_else(|| io::Error::other("--focus UID"))?;
        let target = layout.surfaces.iter()
            .filter(|w| w.owner_uid == uid && !layout.embeds.iter().any(|pair| pair[0] == w.surface_id))
            .max_by_key(|w| w.rect.w as u64 * w.rect.h as u64)
            .ok_or_else(|| io::Error::other("no window of that uid"))?;
        let slots = layout.surfaces.iter()
            .filter(|w| !layout.embeds.iter().any(|pair| pair[0] == w.surface_id))
            .map(|w| WmSlot { surface_id: w.surface_id, rect: w.rect, layer: if w.surface_id == target.surface_id { 0 } else { 1 } })
            .collect();
        wm.wm_declare_layout(&WmDeclareLayoutPayload { slots, focus: target.surface_id, focus_cause: focus_cause::PROGRAM })?;
        wm.wm_enumerate()?;
        println!("focus {}", target.surface_id);
        return Ok(());
    }
    let settings = std::env::args().nth(1).as_deref() == Some("--settings");
    let mut targets = layout.surfaces.iter().filter(|w| {
        if settings {
            w.owner_uid == 50005 && w.rect.w == 1040 && w.rect.h == 720
        } else {
            w.owner_uid == 65030
        }
    });
    let target = targets
        .next()
        .ok_or_else(|| io::Error::other("fixture window unavailable"))?;
    if targets.next().is_some() {
        return Err(io::Error::other("ambiguous fixture window"));
    }
    let slots = layout
        .surfaces
        .iter()
        .filter(|w| !layout.embeds.iter().any(|pair| pair[0] == w.surface_id))
        .map(|w| {
            let mut rect = w.rect;
            // The 1280x840 gate has no Forum process to reserve the top strip.
            if w.owner_uid == 50005 { rect.y = rect.y.max(24); }
            let layer = if w.surface_id == target.surface_id {
                if settings {
                    // Centre Settings horizontally, leaving Mail's header above it.
                    rect.x = (1280 - rect.w) / 2;
                    rect.y = 72;
                } else {
                    rect.x = 360;
                    // Cross the live page's bottom edge so the occlusion gate
                    // sees both exposed dashes and the part behind this sheet.
                    rect.y = 320;
                }
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
        focus: target.surface_id,
        focus_cause: focus_cause::PROGRAM,
    })?;
    wm.wm_enumerate()?;
    Ok(())
}
