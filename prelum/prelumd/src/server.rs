//! `CLASS_PRINT` (prelum_proto::print): one thread per client connection,
//! the caller known by its uid (getpeereid), each seeing only its own
//! printers and jobs.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use aqueduct::{flag, Connection};
use prelum_proto::print::{self, op, reply, status, CLASS};
use serde::de::DeserializeOwned;

use crate::daemon::Daemon;

fn arg<T: DeserializeOwned>(p: &[u8]) -> Result<T, (u8, String)> {
    postcard::from_bytes(p).map_err(|_| (status::MALFORMED, "malformed request".into()))
}

pub fn serve(stream: UnixStream, d: Arc<Daemon>) -> io::Result<()> {
    let (uid, _) = portcullis_peer::uid_gid(stream.as_raw_fd())?;
    let mut conn = Connection::wrap(stream)?;
    loop {
        let m = conn.recv_message()?;
        if m.opcode_class != CLASS {
            continue;
        }
        let p = &m.payload;
        let out = match m.op {
            op::PRINTERS => reply::<Vec<print::Printer>>(Ok(d.printers(uid))),
            op::ADD => reply(arg::<print::Add>(p).and_then(|a| d.add(uid, &a))),
            op::REMOVE => reply(arg::<u32>(p).and_then(|id| d.remove(uid, id))),
            op::RENAME => reply(arg::<print::Rename>(p).and_then(|r| d.rename(uid, &r))),
            op::REPIN => reply(arg::<u32>(p).and_then(|id| d.repin(uid, id))),
            op::IDENTIFY => reply(arg::<u32>(p).and_then(|id| d.identify(uid, id))),
            op::REFRESH => reply(arg::<u32>(p).and_then(|id| d.refresh(uid, id))),
            op::NEARBY => reply(arg::<u8>(p).and_then(|secs| d.nearby(uid, secs))),
            op::PRINT => {
                let fd = conn.take_fds(1).and_then(|mut v| v.pop());
                reply(match (arg::<print::Print>(p), fd) {
                    (Ok(req), Some(fd)) => d.submit(uid, req, fd),
                    (Err(e), _) => Err(e),
                    (_, None) => Err((status::MALFORMED, "no document".into())),
                })
            }
            op::JOBS => reply::<Vec<print::Job>>(Ok(d.jobs(uid))),
            op::CANCEL => reply(arg::<u64>(p).and_then(|j| d.cancel(uid, j))),
            op::RELEASE => reply(arg::<u64>(p).and_then(|j| d.release(uid, j))),
            _ => reply::<String>(Err((status::MALFORMED, "unknown request".into()))),
        };
        conn.send_message(CLASS, m.op, flag::IS_RESPONSE, &out)?;
    }
}
