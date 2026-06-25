//! Debug: send one session verb to the broker as the current uid — to prove the
//! session_launch gate, the registry bound, and PAM VerifyCredential. Usage:
//!   sesstest <sock> launch <component_id> <owner>
//!   sesstest <sock> verify <user> <password>
use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
use std::os::unix::net::UnixStream;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut s = UnixStream::connect(&a[1]).unwrap();
    let _ = round_trip(&mut s, &Request::Hello { version: PROTO_VERSION });
    let req = match a[2].as_str() {
        "verify" => Request::VerifyCredential { user: a[3].clone(), password: a[4].clone() },
        _ => Request::LaunchSessionComponent { component_id: a[3].clone(), owner_name: a[4].clone() },
    };
    let r: Response = round_trip(&mut s, &req).unwrap();
    println!("{r:?}");
}
