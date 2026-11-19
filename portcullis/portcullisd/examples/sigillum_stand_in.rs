//! A stand-in for sigillumd's client socket in VM gates that have no
//! security key or passcode sheet (scripts/vm-apps-access-check.sh): it
//! answers each `VerifyUser` as confirmed by passcode, or refused while
//! `<socket>.refuse` exists, and logs what it was asked. sigillumd's own
//! rules for these asks are tested in sigillum/sigillumd/tests.
//!
//! For an SSH-signing request, `<socket>.security-key` selects a KeyUv reply.
//!
//!   sigillum_stand_in <socket>

use std::os::unix::net::UnixListener;
use std::path::Path;

use sigillum_proto::client::{Method, Purpose, Refusal, Reply, Request, Strength, MAX_MESSAGE};

fn main() {
    let path = std::env::args().nth(1).expect("usage: sigillum_stand_in <socket>");
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).expect("bind");
    let refuse = format!("{path}.refuse");
    let security_key = format!("{path}.security-key");
    for s in l.incoming() {
        let Ok(mut s) = s else { continue };
        let Ok(Some(req)) = sigillum_proto::read_msg::<Request>(&mut s, MAX_MESSAGE) else { continue };
        let reply = match req {
            Request::VerifyUser(v) => {
                let no = Path::new(&refuse).exists();
                println!("sigillum-stand-in: verify {} for {:?}: {}", v.user, v.purpose, if no { "refused" } else { "confirmed" });
                if no {
                    Reply::Refused { ceremony: v.ceremony, reason: Refusal::NotAllowed }
                } else if matches!(v.purpose, Purpose::ArcaSshSign { .. }) {
                    let (method, strength) = if Path::new(&security_key).exists() {
                        (Method::SecurityKey, Strength::KeyUv)
                    } else {
                        (Method::Passcode, Strength::Passcode)
                    };
                    Reply::Verified { ceremony: v.ceremony, method, strength }
                } else {
                    Reply::Verified { ceremony: v.ceremony, method: Method::Passcode, strength: Strength::BiometricSecure }
                }
            }
            other => {
                println!("sigillum-stand-in: not answered: {other:?}");
                continue;
            }
        };
        let _ = sigillum_proto::write_msg(&mut s, &reply, MAX_MESSAGE);
    }
}
