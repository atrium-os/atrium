//! Place and focus two documents in the 1280x720 Sheets desktop fixture.
use fresco_client::Connection;
use fresco_protocol::{focus_cause, WmDeclareLayoutPayload, WmRect, WmSlot};
use std::{env, io, time::Duration};

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let mut wm = Connection::connect("wm.sock")?;
    wm.set_read_timeout(Some(Duration::from_secs(5)))?;
    let (slots, focus) = match args.as_slice() {
        [option] if option == "--clear-focus" => (Vec::new(), 0),
        [option, id, width] if option == "--launcher-width" => {
            let id = id.parse().expect("launcher window ID");
            let width = width.parse().expect("launcher width");
            let windows = wm.wm_enumerate()?;
            let launcher = windows.iter().find(|s| s.surface_id == id).expect("launcher is open");
            let slot = WmSlot {
                surface_id: id,
                rect: WmRect { w: width, ..launcher.rect },
                layer: 0,
            };
            (vec![slot], id)
        }
        [left, right, focus] => {
            let left: u32 = left.parse().expect("plain window ID");
            let right: u32 = right.parse().expect("protected window ID");
            let focus: u32 = focus.parse().expect("focused window ID");
            assert_ne!(left, right);
            assert!(focus == left || focus == right);
            let mut placed = 0;
            let slots = wm
                .wm_enumerate()?
                .into_iter()
                .map(|s| {
                    let mut slot = WmSlot {
                        surface_id: s.surface_id,
                        rect: s.rect,
                        layer: 1,
                    };
                    if s.surface_id == left || s.surface_id == right {
                        let x = if s.surface_id == left { 0 } else { 640 };
                        slot.rect = WmRect {
                            x,
                            y: 0,
                            w: 640,
                            h: 720,
                        };
                        slot.layer = 0;
                        placed += 1;
                    }
                    slot
                })
                .collect();
            assert_eq!(placed, 2, "both documents must be open");
            (slots, focus)
        }
        _ => panic!("expected plain, protected and focused IDs, --launcher-width ID WIDTH, or --clear-focus"),
    };
    wm.wm_declare_layout(&WmDeclareLayoutPayload {
        slots,
        focus,
        focus_cause: focus_cause::PROGRAM,
    })?;
    wm.wm_enumerate()?; // Wait for the compositor to process the declaration.
    Ok(())
}
