//! Print device-add/remove events as they arrive. Try plugging /
//! unplugging a USB device while this runs.

use atrium_devevents::{DeviceWatcher, Event};

fn main() -> std::io::Result<()> {
    let w = DeviceWatcher::open()?;
    eprintln!("listening on devd seqpacket pipe; ^C to exit");
    loop {
        match w.recv()? {
            Event::Added   { devnode } => println!("+ {devnode}"),
            Event::Removed { devnode } => println!("- {devnode}"),
            Event::Other   { raw }     => println!("? {raw}"),
        }
    }
}
