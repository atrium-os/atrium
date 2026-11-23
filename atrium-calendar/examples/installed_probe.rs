//! Guest gate only. Run at the actual app UID inside each installed UI jail.
use aqueduct::{classes::CLASS_CALENDAR, flag, Header};
use atrium_calendar::wire::{call, Request};
use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [socket, date, expected] = args.as_slice() else {
        return Err("expected socket, date and title (or --denied)".into());
    };
    let request = Request::Day {
        date: date.parse()?,
    };
    if expected == "--denied" {
        // The fixture temporarily grants connect permission. Prove that
        // Fasti itself closes the connection without returning a single byte.
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let body = serde_json::to_vec(&request)?;
        let mut frame = Header::new(
            CLASS_CALENDAR,
            1,
            flag::RESPONSE_EXPECTED,
            body.len() as u32,
        )
        .encode()
        .to_vec();
        frame.extend(body);
        if let Err(e) = stream.write_all(&frame) {
            if !matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) {
                return Err(e.into());
            }
        }
        match stream.read(&mut [0]) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            other => return Err(format!("expected peer refusal, got {other:?}").into()),
        }
    } else {
        let day = call(Path::new(socket), &request)?;
        if day.revision != 1 || day.entries.len() != 1 || day.entries[0].title != *expected {
            return Err("incorrect synthetic day or revision".into());
        }
    }
    Ok(())
}
