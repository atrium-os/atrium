//! govreap — debug/operator tool: send a single GovernReap to the
//! portcullisd broker as the current uid, exactly as memoryd's
//! `broker_reap` does (Connect → Hello → GovernReap). Lets an operator
//! (or a post-boot test) exercise the jailed-governor act path
//! memoryd → portcullisd (cap-check) → jaild (reap) WITHOUT inducing
//! real memory pressure. Must run as a uid that owns a services.d
//! manifest granting `memory_govern` (e.g. _memoryd), else portcullisd
//! rejects it — which is itself a useful negative test.
//!
//! Usage: govreap <broker-sock> <jail-name> <trim|exit|kill>

use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use portcullis_ipc::{round_trip, GovSignal, Request, Response, PROTO_VERSION};

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 4 {
        eprintln!("usage: govreap <broker-sock> <jail-name> <trim|exit|kill>");
        return ExitCode::from(2);
    }
    let (sock, jail) = (&a[1], a[2].clone());
    let sig = match a[3].as_str() {
        "trim" => GovSignal::Trim,
        "exit" => GovSignal::Exit,
        "kill" => GovSignal::Kill,
        other => {
            eprintln!("bad signal {other:?} (want trim|exit|kill)");
            return ExitCode::from(2);
        }
    };

    let mut s = match UnixStream::connect(sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect {sock}: {e}");
            return ExitCode::from(1);
        }
    };
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        other => {
            eprintln!("handshake: {other:?}");
            return ExitCode::from(2);
        }
    }
    let req = Request::GovernReap { jail_name: jail.clone(), signal: sig };
    match round_trip(&mut s, &req) {
        Ok(Response::Ok) => {
            eprintln!("govreap: broker accepted GovernReap({jail}, {:?}) -> Ok", a[3]);
            ExitCode::SUCCESS
        }
        Ok(Response::Error { message }) => {
            eprintln!("govreap: broker rejected: {message}");
            ExitCode::from(3)
        }
        Ok(other) => {
            eprintln!("govreap: unexpected {other:?}");
            ExitCode::from(4)
        }
        Err(e) => {
            eprintln!("govreap io: {e}");
            ExitCode::from(5)
        }
    }
}
