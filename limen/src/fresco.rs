//! The real [`Compositor`](crate::daemon::Compositor): frescod's slot op on
//! its activation socket (fresco_protocol::slot, limen.md §5.1). frescod
//! binds slots only for uids on `FRESCOD_SLOT_UIDS` (default `_limen`).

use std::path::PathBuf;
use std::time::Duration;

use aqueduct::{flag, Connection};
use fresco_protocol::slot::{self, SlotBind, SlotBindResult};

use crate::catalogue::SlotKind;
use crate::daemon::Compositor;
use crate::proto::WindowToken;

/// frescod's default activation socket.
pub const DEFAULT_SOCKET: &str = "/var/run/atrium/fresco-activation.sock";

pub struct Frescod {
    path: PathBuf,
    conn: Option<Connection>,
}

impl Frescod {
    pub fn new(path: impl Into<PathBuf>) -> Self { Self { path: path.into(), conn: None } }

    fn ask(&mut self, b: &SlotBind) -> std::io::Result<SlotBindResult> {
        if self.conn.is_none() {
            self.conn = Some(Connection::connect(&self.path)?);
        }
        let c = self.conn.as_mut().unwrap();
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let payload = postcard::to_stdvec(b).map_err(|e| bad(e.to_string()))?;
        c.send_message(aqueduct::classes::CLASS_DISPLAY, slot::op::BIND, flag::RESPONSE_EXPECTED, &payload)?;
        loop {
            let m = c.recv_message_or_timeout(Duration::from_secs(1))?
                .ok_or_else(|| bad("frescod did not answer".into()))?;
            if m.op == slot::op::BIND && m.flags & flag::IS_RESPONSE != 0 {
                return postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()));
            }
        }
    }
}

impl Compositor for Frescod {
    fn bind(&mut self, parent: WindowToken, child: WindowToken, kind: SlotKind, rect: Option<[f32; 4]>) -> Result<(u32, u32), u8> {
        let kind = match kind {
            SlotKind::Plain => slot::kind::PLAIN,
            SlotKind::Credential => slot::kind::CREDENTIAL,
        };
        match self.ask(&SlotBind { parent, child, kind, rect }) {
            Ok(SlotBindResult::Ok { parent_uid, child_uid }) => Ok((parent_uid, child_uid)),
            Ok(SlotBindResult::Refused { reason }) => Err(reason),
            Err(e) => {
                eprintln!("limend: frescod slot op: {e}");
                self.conn = None;
                Err(u8::MAX)
            }
        }
    }
}
