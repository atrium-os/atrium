//! The clipboard, through Tabula (docs/spec/tabula.md): paste on
//! Ctrl+Shift+V (bracketed when the program asked for it), and programs'
//! OSC 52 writes. Every call spends the activation token of a press
//! frescod minted; Tabula checks the press may be spent on it (§2.2), so a
//! program can never copy on its own: it needs the person's press.
//!
//! The same calls as Pergola's clipboard (pergola/src/clipboard.rs); the
//! Terminal isn't a Pergola app, so it carries its own small client.

use std::time::Duration;

use aqueduct::flag;
use tabula::proto::{op, Format, Get, Reply, Set, CLASS};

#[derive(Default)]
pub struct Tabula {
    conn: Option<aqueduct::Connection>,
}

impl Tabula {
    fn conn(&mut self) -> Result<&mut aqueduct::Connection, String> {
        if self.conn.is_none() {
            let path = std::env::var("TABULA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/clipboard.sock".into());
            let c = aqueduct::Connection::connect(&path).map_err(|e| format!("the clipboard: {e}"))?;
            self.conn = Some(c);
        }
        Ok(self.conn.as_mut().unwrap())
    }

    fn call<T: serde::Serialize>(&mut self, o: u16, req: &T) -> Result<Reply, String> {
        let r = (|| {
            let c = self.conn()?;
            let payload = postcard::to_stdvec(req).map_err(|e| e.to_string())?;
            c.send_message(CLASS, o, flag::RESPONSE_EXPECTED, &payload).map_err(|e| e.to_string())?;
            loop {
                let m = c.recv_message_or_timeout(Duration::from_secs(2)).map_err(|e| e.to_string())?
                    .ok_or_else(|| "the clipboard did not answer".to_string())?;
                if m.flags & flag::IS_RESPONSE != 0 && m.op == o {
                    return postcard::from_bytes(&m.payload).map_err(|e| e.to_string());
                }
            }
        })();
        if r.is_err() {
            self.conn = None;
        }
        r
    }

    /// Copy `text` as `text/plain`, spending `token`.
    pub fn copy(&mut self, token: [u8; 16], text: &str) -> Result<(), String> {
        let hash = self.conn()?.upload_blob(text.as_bytes()).map_err(|e| e.to_string())?;
        let set = Set { token, formats: vec![Format { mime: "text/plain".into(), hash }], sensitive: false };
        match self.call(op::SET, &set)? {
            Reply::Ok => Ok(()),
            Reply::Refused { reason, detail } => Err(format!("refused ({reason}/{detail})")),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }

    /// The clipboard's text, spending `token` (a paste press).
    pub fn paste(&mut self, token: [u8; 16]) -> Result<Option<String>, String> {
        let get = Get { token, accept: vec!["text/plain".into()] };
        match self.call(op::GET, &get)? {
            Reply::Got { format } => {
                let bytes = self.conn()?.cache_get(&format.hash).ok_or("the pasted text went missing")?;
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            Reply::Empty => Ok(None),
            Reply::Refused { reason, detail } => Err(format!("refused ({reason}/{detail})")),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }
}

/// What a paste sends to the program: the text with line ends as CR (what
/// Enter sends), control characters other than tab and CR removed so a
/// paste can't carry escape sequences, and the bracketed-paste markers
/// when the program asked for them.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body: String = text
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .chars()
        .filter(|&c| !c.is_control() || c == '\t' || c == '\r')
        .collect();
    let mut out = Vec::with_capacity(body.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    out.extend_from_slice(body.as_bytes());
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_is_bracketed_and_cleaned() {
        assert_eq!(paste_bytes("a\nb", false), b"a\rb");
        assert_eq!(paste_bytes("a\r\nb", true), b"\x1b[200~a\rb\x1b[201~");
        // An embedded end marker can't end the bracket early.
        assert_eq!(paste_bytes("x\x1b[201~rm -rf\n", true), b"\x1b[200~x[201~rm -rf\r\x1b[201~");
        assert_eq!(paste_bytes("tab\there", false), b"tab\there");
    }
}
