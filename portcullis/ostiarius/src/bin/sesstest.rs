//! Debug: send one LaunchSessionComponent to the broker as the current uid —
//! to prove the session_launch gate + the registry bound. Usage:
//!   sesstest <sock> <component_id> <owner>
use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
use std::os::unix::net::UnixStream;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut s = UnixStream::connect(&a[1]).unwrap();
    let _ = round_trip(&mut s, &Request::Hello { version: PROTO_VERSION });
    let r = round_trip(&mut s, &Request::LaunchSessionComponent {
        component_id: a[2].clone(), owner_name: a[3].clone() });
    println!("{r:?}");
}
