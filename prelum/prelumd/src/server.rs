//! `CLASS_PRINT` (prelum_proto::print): one thread per client connection,
//! the caller known by its uid (getpeereid), each seeing only its own
//! printers and jobs. prelum-sheet's connection says first, with `FOR`,
//! which app it is open for; from then on it acts for that app's person.
//! Curia and prelum-status act for their own person, each with the ops its
//! part needs ([`Caller`]); any other app is refused.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use aqueduct::{flag, Connection};
use prelum_proto::print::{self, op, reply, status, CLASS};
use serde::de::DeserializeOwned;

use crate::daemon::Daemon;
use crate::sheet::Caller;

fn arg<T: DeserializeOwned>(p: &[u8]) -> Result<T, (u8, String)> {
    postcard::from_bytes(p).map_err(|_| (status::MALFORMED, "malformed request".into()))
}

pub fn serve(stream: UnixStream, d: Arc<Daemon>) -> io::Result<()> {
    let (peer, _) = portcullis_peer::uid_gid(stream.as_raw_fd())?;
    let mut conn = Connection::wrap(stream)?;
    // Whose printers and jobs: the peer's own, or after FOR, the person's.
    let mut uid = peer;
    let mut persona = String::new();
    let mut acting = false;
    let sheet = d.cfg().sheet_uid == Some(peer);
    let caller = if sheet { Caller::Person } else { d.caller(peer) };
    match &caller {
        Caller::Curia { person, persona: p } => {
            uid = *person;
            persona = p.clone();
        }
        Caller::Status { person } => uid = *person,
        _ => {}
    }
    loop {
        let m = conn.recv_message()?;
        if m.opcode_class != CLASS {
            continue;
        }
        let p = &m.payload;
        // The sheet acts only for an app: nothing before FOR.
        if sheet && !acting && m.op != op::FOR {
            let _ = conn.take_fds(1);
            let out = reply::<String>(Err((status::NOT_ALLOWED, "say which app first".into())));
            conn.send_message(CLASS, m.op, flag::IS_RESPONSE, &out)?;
            continue;
        }
        // FOR answers for itself (only the sheet may); the rest by caller.
        if !sheet && m.op != op::FOR && !caller.may(m.op) {
            let _ = conn.take_fds(1);
            let msg = if caller == Caller::App { "apps print through the print sheet" } else { "not for this app" };
            let out = reply::<String>(Err((status::NOT_ALLOWED, msg.into())));
            conn.send_message(CLASS, m.op, flag::IS_RESPONSE, &out)?;
            continue;
        }
        // A sheet represents the app's persona and only needs picker operations.
        if sheet && acting && !matches!(m.op, op::FOR | op::PRINTERS | op::ADD | op::NEARBY | op::PREVIEW | op::SAVE_PDF | op::PRINT | op::PRINT_PIN | op::PIN_BINDING) {
            let _ = conn.take_fds(1);
            let out = reply::<String>(Err((status::NOT_ALLOWED, "not for the print sheet".into())));
            conn.send_message(CLASS, m.op, flag::IS_RESPONSE, &out)?;
            continue;
        }
        let scope = sheet.then_some(persona.as_str());
        let mut back: Option<std::fs::File> = None;
        let out = match m.op {
            op::FOR if acting => reply::<String>(Err((status::NOT_ALLOWED, "already acting for an app".into()))),
            op::FOR => reply(arg::<u32>(p).and_then(|app| d.act_for(peer, app)).map(|(person, p)| {
                uid = person;
                acting = true;
                persona = p.clone();
                p
            })),
            op::PREVIEW => {
                let fd = conn.take_fds(1).and_then(|mut v| v.pop());
                reply(match (arg::<print::PreviewReq>(p), fd) {
                    (Ok(req), Some(fd)) => d.preview(&req, fd),
                    (Err(e), _) => Err(e),
                    (_, None) => Err((status::MALFORMED, "no document".into())),
                })
            }
            op::SAVE_PDF => {
                let fd = conn.take_fds(1).and_then(|mut v| v.pop());
                reply(match (arg::<print::Layout>(p), fd) {
                    (Ok(l), Some(fd)) => d.save_pdf(&l, fd).map(|(n, f)| {
                        back = Some(f);
                        n
                    }),
                    (Err(e), _) => Err(e),
                    (_, None) => Err((status::MALFORMED, "no document".into())),
                })
            }
            op::PRINTERS => reply::<Vec<print::Printer>>(Ok(d.printers(uid, scope))),
            op::ADD => reply(arg::<print::Add>(p).and_then(|a| d.add(uid, &persona, &a))),
            op::REMOVE => reply(arg::<u32>(p).and_then(|id| d.remove(uid, id))),
            op::RENAME => reply(arg::<print::Rename>(p).and_then(|r| d.rename(uid, &r))),
            op::REPIN => reply(arg::<u32>(p).and_then(|id| d.repin(uid, id))),
            op::IDENTIFY => reply(arg::<u32>(p).and_then(|id| d.identify(uid, id))),
            op::REFRESH => reply(arg::<u32>(p).and_then(|id| d.refresh(uid, id))),
            op::NEARBY => reply(arg::<u8>(p).and_then(|secs| d.nearby(uid, scope, secs))),
            op::SHARE => reply(arg::<print::Share>(p).and_then(|s| d.share(uid, &s))),
            op::HISTORY => reply::<Vec<print::Ended>>(Ok(d.history(uid))),
            op::CLEAR_HISTORY => reply(d.clear_history(uid)),
            op::WATCH => reply(arg::<u64>(p).map(|after| d.watch(uid, after))),
            op::PRINT => {
                let fd = conn.take_fds(1).and_then(|mut v| v.pop());
                reply(match (arg::<print::Print>(p), fd) {
                    (Ok(req), Some(fd)) => d.submit(uid, &persona, req, fd),
                    (Err(e), _) => Err(e),
                    (_, None) => Err((status::MALFORMED, "no document".into())),
                })
            }
            op::PIN_BINDING if sheet => reply(arg::<u32>(p).and_then(|printer| d.pin_binding(uid, &persona, printer))),
            op::PRINT_PIN => {
                let fd = conn.take_fds(1).and_then(|mut v| v.pop());
                reply(match (arg::<(print::Print, [u8; 16])>(p), fd) {
                    (Ok((req, handle)), Some(fd)) => d.submit_pin(uid, &persona, req, fd, handle),
                    (Err(e), _) => Err(e),
                    (_, None) => Err((status::MALFORMED, "no document".into())),
                })
            }
            op::JOBS => reply::<Vec<print::Job>>(Ok(d.jobs(uid))),
            op::CANCEL => reply(arg::<u64>(p).and_then(|j| d.cancel(uid, j))),
            op::RELEASE => reply(arg::<u64>(p).and_then(|j| d.release(uid, j))),
            _ => reply::<String>(Err((status::MALFORMED, "unknown request".into()))),
        };
        match back {
            Some(f) => conn.send_message_with_fds(CLASS, m.op, flag::IS_RESPONSE, &out, &[f.as_raw_fd()])?,
            None => conn.send_message(CLASS, m.op, flag::IS_RESPONSE, &out)?,
        }
    }
}
