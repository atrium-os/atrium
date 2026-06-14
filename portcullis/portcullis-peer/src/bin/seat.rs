//! `atrium-seat` — bind / query the active session (the seat: which human
//! session owns the shared engines now).
//!
//!   atrium-seat active            # print the active session (empty if none)
//!   atrium-seat set <user>        # bind <user>'s session to the engines (login)
//!   atrium-seat is-active <user>  # exit 0 if active, 1 if not
//!
//! In the common case the boot-time login calls `set` once; logout would clear
//! it. Fast-user-switching re-`set`s. Services don't shell out to this — they
//! call `portcullis_peer::seat::*` directly; the CLI is for login + operators.

use portcullis_peer::seat;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("active") => match seat::active() {
            Some(u) => println!("{u}"),
            None => {}
        },
        Some("set") => match args.get(2) {
            Some(user) => {
                if let Err(e) = seat::set_active(user) {
                    eprintln!("atrium-seat: set {user}: {e} (is {} writable?)", seat::ACTIVE_SESSION);
                    exit(1);
                }
                eprintln!("atrium-seat: active session bound to '{user}'");
            }
            None => { eprintln!("usage: atrium-seat set <user>"); exit(2); }
        },
        Some("is-active") => match args.get(2) {
            Some(user) => exit(if seat::is_active(user) { 0 } else { 1 }),
            None => { eprintln!("usage: atrium-seat is-active <user>"); exit(2); }
        },
        _ => {
            eprintln!("usage: atrium-seat (active | set <user> | is-active <user>)");
            exit(2);
        }
    }
}
