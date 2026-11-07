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
//! arca-probe SOCK store-for HOST[:PORT] VALUE  # an API token for https://HOST[:PORT]
//! arca-probe SOCK list-for HOST
//! arca-probe - persona PUBHEX APP            # the app's courier persona
//! arca-probe SOCK accounts USER PUBHEX APP # as arca-ui's uid: the chip's list
//! arca-probe SOCK generate USER PUBHEX APP # as arca-ui's uid: a handle
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
    if arg(2) == "persona" {
        let p: [u8; 32] = unhex(arg(3)).try_into().unwrap_or([0; 32]);
        println!("{}", arcad::courier::app_persona(&p, arg(4)));
        return ExitCode::SUCCESS;
    }
    let (st, body) = match arg(2) {
        "store-for" => {
            w.u16(4);
            w.u8(2);
            w.str("probe-token");
            let (h, port) = host_port(arg(3));
            arcad::proto::write_endpoint(&mut w, h, port);
            w.bytes(arg(4).as_bytes());
            call(sock, op::STORE_FOR, &w.buf)
        }
        "list-for" => {
            let (h, port) = host_port(arg(3));
            arcad::proto::write_endpoint(&mut w, h, port);
            call(sock, op::LIST_FOR, &w.buf)
        }
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
        // A system service's bond key in the machine store (§6.4).
        "store-bond" => {
            w.u16(7);
            w.u8(1);
            w.str("probe-bond");
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
        "accounts" | "generate" => {
            w.str(arg(3));
            w.raw(&unhex(arg(4)));
            w.str(arg(5));
            call(sock, if arg(2) == "accounts" { op::U_ACCOUNTS } else { op::U_GENERATE }, &w.buf)
        }
        c => {
            eprintln!("unknown command {c}");
            return ExitCode::from(2);
        }
    };
    println!("status={st} body={}", hex(&body));
    ExitCode::SUCCESS
}

/// `host` or `host:port` (443 by default).
fn host_port(a: &str) -> (&str, u16) {
    match a.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(443)),
        None => (a, 443),
    }
}
