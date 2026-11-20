//! Spending the person's press with frescod (navigator-ui.md §6.4).
//!
//! Save and the hand-off answers act outside the page: a file written, a
//! link handed to another program. navigatord acts only on a press frescod
//! minted in the UI's window and spends here with `purpose::PROMPT`, which
//! frescod grants only if the UI's guarded prompt row was whole, uncovered
//! by any other window and still for 500 ms before the press. So another
//! program can't slide a window over the row, or off it just before the
//! press, to have the person agree to something they did not see.

use std::path::PathBuf;
use std::time::Duration;

use aqueduct::flag;
use fresco_protocol::activation::{op, refusal, ConsumePrompt, ConsumeResult, NONE};

pub struct Frescod {
    path: PathBuf,
    conn: Option<aqueduct::Connection>,
}

impl Frescod {
    pub fn new(path: impl Into<PathBuf>) -> Frescod {
        Frescod { path: path.into(), conn: None }
    }

    fn ask(&mut self, c: ConsumePrompt) -> std::io::Result<ConsumeResult> {
        if self.conn.is_none() {
            self.conn = Some(aqueduct::Connection::connect(&self.path)?);
        }
        let conn = self.conn.as_mut().unwrap();
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let payload = postcard::to_stdvec(&c).map_err(|e| bad(e.to_string()))?;
        conn.send_message(aqueduct::classes::CLASS_DISPLAY, op::CONSUME_PROMPT, flag::RESPONSE_EXPECTED, &payload)?;
        conn.set_read_timeout(Some(Duration::from_millis(500)))?;
        let m = conn.recv_message()?;
        if m.op != op::CONSUME_PROMPT || m.flags & flag::IS_RESPONSE == 0 {
            return Err(bad("unexpected reply".into()));
        }
        postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()))
    }

    /// Spend `press`, presented by the UI running as `uid`: `Ok` or why not,
    /// in words for the UI.
    pub fn spend(&mut self, press: [u8; 16], uid: u32, window: [u8; 16]) -> Result<(), String> {
        if press == NONE {
            return Err("no press from the screen came with it".into());
        }
        match self.ask(ConsumePrompt { press, presenter_uid: uid, window }) {
            Ok(ConsumeResult::Ok { .. }) => Ok(()),
            Ok(ConsumeResult::Refused { reason }) => Err(match reason {
                refusal::WRONG_WINDOW => "the press belongs to another window".into(),
                refusal::OBSCURED => "something was over the question, or it had just moved".into(),
                refusal::EXPIRED => "the press was too long ago".into(),
                refusal::NOT_FOCUSED => "the window lost the focus".into(),
                r => format!("frescod refused the press ({r})"),
            }),
            Err(e) => {
                self.conn = None;
                Err(format!("frescod: {e}"))
            }
        }
    }
}

/// The uid at the other end of `s`.
pub fn peer_uid(s: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid on a connected socket we own, into two locals.
        (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
    }
    #[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
    {
        let mut c: libc::ucred = unsafe { std::mem::zeroed() };
        let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: SO_PEERCRED into a ucred of the size given.
        let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut n) };
        (r == 0).then_some(c.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn guarded_press_must_come_from_this_ui_window() {
        let (client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let mut conn = aqueduct::Connection::wrap(server).unwrap();
            for token in [[1; 16], [2; 16]] {
                let m = conn.recv_message().unwrap();
                assert_eq!(m.op, op::CONSUME_PROMPT);
                let c: ConsumePrompt = postcard::from_bytes(&m.payload).unwrap();
                assert_eq!((c.press, c.presenter_uid), (token, 4101));
                let result = if c.window == [18; 16] { ConsumeResult::Refused { reason: refusal::WRONG_WINDOW } } else {
                    assert_eq!(c.window, [17; 16]);
                    ConsumeResult::Ok { window_id: 42, kind: fresco_protocol::activation::kind::BUTTON,
                    minted_ns: 42, drag_id: 0 }
                };
                conn.send_message(aqueduct::classes::CLASS_DISPLAY, op::CONSUME_PROMPT, flag::IS_RESPONSE,
                    &postcard::to_stdvec(&result).unwrap()).unwrap();
            }
        });
        let mut fresco = Frescod { path: PathBuf::new(), conn: Some(aqueduct::Connection::wrap(client).unwrap()) };
        assert!(fresco.spend(NONE, 4101, [17; 16]).is_err());
        assert!(fresco.spend([1; 16], 4101, [18; 16]).is_err());
        assert!(fresco.spend([2; 16], 4101, [17; 16]).is_ok());
        worker.join().unwrap();
    }
}
