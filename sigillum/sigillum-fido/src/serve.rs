//! The driver's socket loop, shared by the binary and the daemon tests.
//!
//! The `Hello` goes first. A reader thread takes `ToDriver` messages off the
//! socket: a `Cancel` sets the client's cancel flag at once (it reaches the
//! authenticator as `CTAPHID_CANCEL` within one poll), anything else goes to
//! the loop here, which owns the device and answers one request at a time.
//! A touch wait becomes a `NeedTouch` record so the sheet can say "touch
//! your key".

use std::cell::Cell;
use std::io;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use sigillum_ctap::hid::HidDevice;
use sigillum_proto::driver::{DriverError, FromDriver, ToDriver, MAX_MESSAGE};
use sigillum_proto::{read_msg, write_msg};

use crate::Driver;

/// Serve `sigillumd` on `sock` until it closes the socket or the device
/// goes away.
pub fn serve<D: HidDevice>(mut driver: Driver<D>, hello: FromDriver, sock: UnixStream) -> io::Result<()> {
    let mut out = sock.try_clone()?;
    write_msg(&mut out, &hello, MAX_MESSAGE)?;

    let current = Rc::new(Cell::new(0u64));
    {
        let current = current.clone();
        let mut hint = out.try_clone()?;
        driver.on_touch(move || {
            // Best effort: a broken socket shows up on the next real write.
            let _ = write_msg(&mut hint, &FromDriver::NeedTouch { ceremony: current.get() }, MAX_MESSAGE);
        });
    }

    let cancel = driver.cancel_flag();
    let (tx, rx) = mpsc::channel::<ToDriver>();
    let mut input = sock;
    std::thread::spawn(move || loop {
        match read_msg::<ToDriver>(&mut input, MAX_MESSAGE) {
            Ok(Some(ToDriver::Cancel { .. })) => cancel.store(true, Ordering::SeqCst),
            Ok(Some(m)) => {
                if tx.send(m).is_err() {
                    return;
                }
            }
            _ => return, // EOF or a malformed message: the broker is gone
        }
    });

    for req in rx {
        let ceremony = match &req {
            ToDriver::Register { ceremony, .. } | ToDriver::Assert { ceremony, .. } | ToDriver::Cancel { ceremony } => *ceremony,
        };
        current.set(ceremony);
        let Some(rec) = driver.handle(req) else { continue };
        let gone = matches!(rec, FromDriver::Error { error: DriverError::Gone, .. });
        write_msg(&mut out, &rec, MAX_MESSAGE)?;
        if gone {
            break;
        }
    }
    Ok(())
}
