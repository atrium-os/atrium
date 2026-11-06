//! Spending a press with frescod (activation purpose FILL). frescod lets
//! only its consumer uids (`_arca-ui`) reach this socket, mints the token
//! for the press in arca-ui's own slot window, and refuses `OBSCURED`
//! unless that slot was whole, uncovered and still for 500 ms before it.

use std::path::PathBuf;
use std::time::Duration;

use aqueduct::flag;
use fresco_protocol::activation::{op, purpose, refusal, Consume, ConsumeResult};

use crate::field::{Notice, Token};

pub trait Gate {
    /// `Ok(window_id)` of the press, or why not.
    fn fill(&mut self, token: Token, presenter_uid: u32) -> Result<u32, Notice>;
}

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
}

impl Gate for Frescod {
    fn fill(&mut self, token: Token, presenter_uid: u32) -> Result<u32, Notice> {
        if token == fresco_protocol::activation::NONE {
            return Err(Notice::NoPress);
        }
        match self.ask(Consume { token, presenter_uid, purpose: purpose::FILL }) {
            Ok(ConsumeResult::Ok { window_id, .. }) => Ok(window_id),
            Ok(ConsumeResult::Refused { reason: refusal::OBSCURED }) => Err(Notice::Obscured),
            Ok(ConsumeResult::Refused { .. }) => Err(Notice::NoPress),
            Err(_) => {
                self.conn = None;
                Err(Notice::NoPress)
            }
        }
    }
}
