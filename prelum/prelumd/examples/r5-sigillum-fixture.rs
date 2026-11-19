//! Test-only Sigillum endpoint for the R5 desktop gate.
use std::error::Error;
use std::os::unix::net::UnixListener;
use std::time::Duration;

use sigillum_proto::client::{Method, Purpose, Refusal, Reply, Request, Strength, MAX_MESSAGE};
use sigillum_proto::{read_msg, write_msg};

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os().nth(1).ok_or("expected socket path")?;
    let listener = UnixListener::bind(path)?;
    for approved in [false, true] {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let Some(Request::VerifyUser(request)) = read_msg(&mut stream, MAX_MESSAGE)? else {
            return Err("expected VerifyUser".into());
        };
        if request.user != "alice"
            || request.purpose != (Purpose::SharePrinter { printer: "Hall".into() })
            || request.min_strength != Strength::BiometricSecure
        {
            return Err(format!("unexpected verification request: {request:?}").into());
        }
        let ceremony = request.ceremony;
        let reply = if approved {
            Reply::Verified { ceremony, method: Method::Passcode, strength: Strength::Passcode }
        } else {
            Reply::Refused { ceremony, reason: Refusal::NotAllowed }
        };
        write_msg(&mut stream, &reply, MAX_MESSAGE)?;
        eprintln!("Hall for alice: {}", if approved { "verified" } else { "refused" });
    }
    Ok(())
}
