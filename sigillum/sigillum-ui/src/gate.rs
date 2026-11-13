//! Spending a press with frescod (activation purpose `IDENTITY`). frescod
//! lets only its consumer uids (`_sigillum-ui`) reach this socket, minted
//! the token for a press in the sheet's own window, and refuses `OBSCURED`
//! unless the sheet was whole, uncovered and still for 500 ms before it.

use std::path::PathBuf;
use std::time::Duration;

use aqueduct::flag;
use fresco_protocol::activation::{op, purpose, Consume, ConsumeResult};

pub const DEFAULT_SOCKET: &str = "/var/run/atrium/fresco-activation.sock";

pub struct Frescod {
    path: PathBuf,
    conn: Option<aqueduct::Connection>,
}

impl Frescod {
    pub fn new(path: impl Into<PathBuf>) -> Frescod {
        Frescod { path: path.into(), conn: None }
    }

    fn ask(&mut self, c: Consume) -> std::io::Result<ConsumeResult> {
        if self.conn.is_none() {
            self.conn = Some(aqueduct::Connection::connect(&self.path)?);
        }
        let conn = self.conn.as_mut().unwrap();
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let payload = postcard::to_stdvec(&c).map_err(|e| bad(e.to_string()))?;
        conn.send_message(aqueduct::classes::CLASS_DISPLAY, op::CONSUME, flag::RESPONSE_EXPECTED, &payload)?;
        conn.set_read_timeout(Some(Duration::from_millis(500)))?;
        let m = conn.recv_message()?;
        if m.op != op::CONSUME || m.flags & flag::IS_RESPONSE == 0 {
            return Err(bad("unexpected reply".into()));
        }
        postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()))
    }

    /// Whether `token` was a real press in window `window_id`, ours, seen
    /// whole. Any failure is "no".
    pub fn identity(&mut self, token: [u8; 16], me: u32, window_id: u32) -> bool {
        if token == fresco_protocol::activation::NONE {
            return false;
        }
        match self.ask(Consume { token, presenter_uid: me, purpose: purpose::IDENTITY }) {
            Ok(ConsumeResult::Ok { window_id: w, .. }) => w == window_id,
            Ok(_) => false,
            Err(_) => {
                self.conn = None;
                false
            }
        }
    }
}
