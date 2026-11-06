//! `arca-probe` — a test client for arcad, for the VM gates. Run it as
//! different uids (`su -m <user> -c ...`) to see what each identity gets.
//!
//! ```text
//! arca-probe SOCK unlock USER PASSCODE      # as Vestibulum's uid
//! arca-probe SOCK lock USER
//! arca-probe SOCK state USER
//! arca-probe SOCK store VALUE               # prints the new item id (hex)
//! arca-probe SOCK release ID
//! arca-probe SOCK list
//! arca-probe SOCK mint ACCOUNT              # ed25519; prints id and pubkey
//! arca-probe SOCK sign ID MESSAGE
//! ```
//!
//! Prints `status=N` and any body; exits 0 whatever the status, so a
//! script can compare. Never use it on a real user's store: `release`
//! prints the value.

use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use arca_store::codec::Writer;
use arcad::client::VestibulumLink;
use arcad::proto::{self, op};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

fn call(sock: &str, opc: u16, body: &[u8]) -> (u8, Vec<u8>) {
    let mut s = UnixStream::connect(sock).expect("connect");
    proto::write_request(&mut s, opc, body).expect("write");
    let (st, b) = proto::read_reply(&mut s).expect("read");
    (st, b.as_ref().to_vec())
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: arca-probe SOCK CMD ...");
        return ExitCode::from(2);
    }
    let sock = &a[1];
    let arg = |i: usize| a.get(i).map(String::as_str).unwrap_or("");
    let mut w = Writer::new();
    let (st, body) = match arg(2) {
        "unlock" => {
            match VestibulumLink::new(sock).unlock(arg(3), arg(4).as_bytes()) {
                Ok(u) => println!("unlocked {u:?}"),
                Err(e) => println!("error {e:?}"),
            }
            return ExitCode::SUCCESS;
        }
        "lock" => {
            w.str(arg(3));
            call(sock, op::V_LOCK, &w.buf)
        }
        "state" => {
            w.str(arg(3));
            call(sock, op::V_STATE, &w.buf)
        }
        "store" => {
            w.u16(1);
            w.u8(2);
            w.str("probe");
            w.bytes(arg(3).as_bytes());
            call(sock, op::STORE, &w.buf)
        }
        "release" => call(sock, op::RELEASE, &unhex(arg(3))),
        "list" => call(sock, op::LIST, &[]),
        "mint" => {
            w.u16(0x100);
            w.u8(2);
            w.str(arg(3));
            call(sock, op::MINT, &w.buf)
        }
        "sign" => {
            w.raw(&unhex(arg(3)));
            w.bytes(arg(4).as_bytes());
            call(sock, op::SIGN, &w.buf)
        }
        c => {
            eprintln!("unknown command {c}");
            return ExitCode::from(2);
        }
    };
    println!("status={st} body={}", hex(&body));
    ExitCode::SUCCESS
}
