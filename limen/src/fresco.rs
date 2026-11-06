//! The real [`Compositor`](crate::daemon::Compositor): frescod's slot op on
//! its activation socket (fresco_protocol::slot, limen.md §5.1). frescod
//! binds slots only for uids on `FRESCOD_SLOT_UIDS` (default `_limen`).

use std::path::PathBuf;
use std::time::Duration;

use aqueduct::{flag, Connection};
use fresco_protocol::slot::{self, PressedQuery, PressedResult, SlotBind, SlotBindResult};

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
        self.call(slot::op::BIND, b)
    }

    fn call<Q: serde::Serialize, R: serde::de::DeserializeOwned>(&mut self, opc: u16, q: &Q) -> std::io::Result<R> {
        if self.conn.is_none() {
            self.conn = Some(Connection::connect(&self.path)?);
        }
        let c = self.conn.as_mut().unwrap();
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let payload = postcard::to_stdvec(q).map_err(|e| bad(e.to_string()))?;
        c.send_message(aqueduct::classes::CLASS_DISPLAY, opc, flag::RESPONSE_EXPECTED, &payload)?;
        loop {
            let m = c.recv_message_or_timeout(Duration::from_secs(1))?
                .ok_or_else(|| bad("frescod did not answer".into()))?;
            if m.op == opc && m.flags & flag::IS_RESPONSE != 0 {
                return postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()));
            }
        }
    }
}

impl Compositor for Frescod {
    fn bind(&mut self, parent: WindowToken, child: WindowToken, kind: SlotKind, rect: Option<[f32; 4]>, z: u8) -> Result<(u32, u32), u8> {
        let kind = match kind {
            SlotKind::Plain => slot::kind::PLAIN,
            SlotKind::Credential => slot::kind::CREDENTIAL,
        };
        match self.ask(&SlotBind { parent, child, kind, rect, z, keep_shown: false }) {
            Ok(SlotBindResult::Ok { parent_uid, child_uid }) => Ok((parent_uid, child_uid)),
            Ok(SlotBindResult::Refused { reason }) => Err(reason),
            Err(e) => {
                eprintln!("limend: frescod slot op: {e}");
                self.conn = None;
                Err(u8::MAX)
            }
        }
    }

    fn pressed_within(&mut self, window: WindowToken, ms: u32) -> bool {
        let q = PressedQuery { window, within_ms: ms.min(slot::PRESSED_MAX_MS) };
        match self.call::<_, PressedResult>(slot::op::PRESSED, &q) {
            Ok(PressedResult::Ok { pressed }) => pressed,
            Ok(PressedResult::Refused { reason }) => {
                eprintln!("limend: frescod refused the press query ({reason})");
                false
            }
            Err(e) => {
                eprintln!("limend: frescod press query: {e}");
                self.conn = None;
                false
            }
        }
    }

    /// Unbind but keep shown (`keep_shown`), for Forum's `pip` layer.
    fn release(&mut self, parent: WindowToken, child: WindowToken) -> Result<(), u8> {
        // The kind and z don't matter for an unbind.
        match self.ask(&SlotBind { parent, child, kind: slot::kind::PLAIN, rect: None, z: 0, keep_shown: true }) {
            Ok(SlotBindResult::Ok { .. }) => Ok(()),
            Ok(SlotBindResult::Refused { reason }) => Err(reason),
            Err(e) => {
                eprintln!("limend: frescod slot release: {e}");
                self.conn = None;
                Err(u8::MAX)
            }
        }
    }

    fn embed_connection(&mut self, owner_uid: u32) -> Result<std::os::unix::net::UnixStream, u8> {
        self.open_embed(owner_uid).map_err(|e| {
            eprintln!("limend: frescod embed connection: {e}");
            u8::MAX
        })
    }
}

impl Frescod {
    /// Open a new activation-socket connection, ask for EMBED_CONNECT, and
    /// hand back the raw stream, which frescod then treats as the child's
    /// client connection. Read exactly the reply and nothing more, and send
    /// nothing after it.
    pub fn open_embed(&self, owner_uid: u32) -> std::io::Result<std::os::unix::net::UnixStream> {
        use aqueduct::envelope::{read_message_alloc, write_message};
        use slot::{EmbedConnect, EmbedConnectResult, op::EMBED_CONNECT as OP};
        let mut s = std::os::unix::net::UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        let payload = postcard::to_stdvec(&EmbedConnect { owner_uid }).unwrap();
        let h = aqueduct::Header::new(aqueduct::classes::CLASS_DISPLAY, OP, flag::RESPONSE_EXPECTED, payload.len() as u32);
        write_message(&mut s, h, &payload)?;
        let (h, body) = read_message_alloc(&mut s)?;
        let bad = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        if h.op != OP || h.flags & flag::IS_RESPONSE == 0 {
            return Err(bad(format!("unexpected reply op {:#x}", h.op)));
        }
        match postcard::from_bytes(&body).map_err(|e| bad(e.to_string()))? {
            EmbedConnectResult::Ok => {
                s.set_read_timeout(None)?;
                Ok(s)
            }
            EmbedConnectResult::Refused { reason } => Err(bad(format!("refused ({reason})"))),
        }
    }
}
