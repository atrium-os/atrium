//! The chrome channel (F4d, docs/spec/fresco-f4-split.md): how the
//! Navigator, which is trusted, tells the broker what to show in the chrome
//! it draws over a window, without the message passing through the window's
//! renderer.
//!
//! The Navigator does not connect to `fresco.sock` for a document. It asks
//! the broker over the chrome socket (`OPEN_CLIENT`), and the broker answers
//! `OPENED` with a connection handle and the client end of a fresh fresco
//! connection as the fd. The broker serves that connection with a renderer
//! of its own, like any other. So the Navigator knows, without trusting
//! anything the document said, which renderer's windows a handle names, and
//! `SET_ORIGIN` names a window by (handle, the window id the client sees).
//!
//! Same framing as the renderer records ([`crate::chan`], 96 bytes):
//! word 0 kind, word 1 handle, word 2 window, bytes 32..96 the origin.

use crate::{Rec, Refused, MSG_BYTES, TITLE_BYTES};

/// Longest origin the chrome shows.
pub const ORIGIN_BYTES: usize = TITLE_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChromeMsg {
    /// Navigator → broker: open a fresco connection for a document.
    OpenClient,
    /// Broker → Navigator: the connection's handle; its client end is the fd.
    Opened { handle: u32 },
    /// Navigator → broker: show `origin` on `window` of connection `handle`
    /// (empty: show none).
    SetOrigin { handle: u32, window: u32, origin: String },
}

mod kind {
    pub const OPEN_CLIENT: u32 = 0x201;
    pub const OPENED: u32 = 0x202;
    pub const SET_ORIGIN: u32 = 0x203;
}

/// Characters an origin may hold: what a URL origin is made of once the
/// host is in punycode (`xn--…`). Anything else is refused, so no
/// look-alike letters, controls or spaces reach the screen.
pub fn origin_char(c: u8) -> bool {
    matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b':' | b'/' | b'-' | b'[' | b']' | b'_')
}

impl ChromeMsg {
    pub fn encode(&self) -> [u8; MSG_BYTES] {
        use kind::*;
        let r = match self {
            ChromeMsg::OpenClient => Rec::new(OPEN_CLIENT, 0),
            ChromeMsg::Opened { handle } => Rec::new(OPENED, *handle),
            ChromeMsg::SetOrigin { handle, window, origin } => {
                let mut r = Rec::new(SET_ORIGIN, *handle);
                r.put(2, *window);
                r.title(origin);
                r
            }
        };
        r.0
    }

    /// Parse a record: known kind, fields in range, unused bytes zero.
    pub fn decode(bytes: &[u8]) -> Result<ChromeMsg, Refused> {
        use kind::*;
        let b: [u8; MSG_BYTES] = bytes.try_into().map_err(|_| Refused("record size"))?;
        let r = Rec(b);
        let (k, handle, window) = (r.get(0), r.get(1), r.get(2));
        let words_used = match k {
            OPEN_CLIENT => 1,
            OPENED => 2,
            SET_ORIGIN => 3,
            _ => return Err(Refused("unknown kind")),
        };
        if (words_used..8).any(|i| r.get(i) != 0) {
            return Err(Refused("unused field not zero"));
        }
        let text = &r.0[32..];
        let n = text.iter().position(|&c| c == 0).unwrap_or(ORIGIN_BYTES);
        if text[n..].iter().any(|&c| c != 0) || (k != SET_ORIGIN && n != 0) {
            return Err(Refused("bytes after the origin"));
        }
        Ok(match k {
            OPEN_CLIENT => ChromeMsg::OpenClient,
            OPENED => ChromeMsg::Opened { handle },
            _ => {
                if window == 0 || window > u16::MAX as u32 {
                    return Err(Refused("window id out of range"));
                }
                if !text[..n].iter().all(|&c| origin_char(c)) {
                    return Err(Refused("origin holds a character the chrome does not show"));
                }
                let origin = String::from_utf8(text[..n].to_vec()).expect("ASCII");
                ChromeMsg::SetOrigin { handle, window, origin }
            }
        })
    }
}
