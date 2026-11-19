//! Verify a printer sharing change in Sigillum before committing it.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use sigillum_proto::client::{Purpose, Reply, Request, Strength, VerifyUser, MAX_MESSAGE};

pub(crate) fn verify(path: &Path, ceremony: u64, user: &str, printer: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(path)
        .map_err(|e| format!("Sigillum verification is unavailable ({e})"))?;
    // Sigillum's ceremony has its own timeout. Bound the wait even if it fails.
    stream.set_read_timeout(Some(Duration::from_secs(11 * 60))).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let min = Strength::BiometricSecure;
    let request = Request::VerifyUser(VerifyUser {
        ceremony,
        user: user.into(),
        purpose: Purpose::SharePrinter { printer: printer.into() },
        min_strength: min,
    });
    sigillum_proto::write_msg(&mut stream, &request, MAX_MESSAGE)
        .map_err(|e| format!("Could not ask Sigillum to verify ({e})"))?;
    match sigillum_proto::read_msg::<Reply>(&mut stream, MAX_MESSAGE) {
        Ok(Some(Reply::Verified { ceremony: id, strength, .. })) if id == ceremony && strength >= min => Ok(()),
        Ok(Some(Reply::Refused { ceremony: id, reason })) if id == ceremony => {
            Err(format!("Sigillum verification was refused ({reason:?})"))
        }
        Ok(_) => Err("Sigillum did not verify printer sharing".into()),
        Err(e) => Err(format!("Could not verify printer sharing ({e})")),
    }
}
