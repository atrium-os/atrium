//! forum-ctl — the intent wire between Forum's chrome apps and the WM core
//! (`docs/spec/forum.md` §3-4).
//!
//! The decomposed-shell contract: only `forum-wm` holds `window-management`. The
//! visible chrome — `forum-dock`, `forum-bar`, `forum-shelf`, `forum-overview` — are
//! ordinary `graphics`-only Insula apps. They never touch another app's surface
//! directly; they ask the core for *intents* over this wire, and the core (which
//! holds the cap) authorizes and carries them out against Fresco. So a bug in the
//! dock/shelf/overview can't manipulate windows — it never held the power.
//!
//! Wire: postcard-encoded `Intent` (chrome → core) and `Reply` (core → chrome).

use serde::{Deserialize, Serialize};

pub use fresco_protocol::WmSurfaceInfo;

/// A chrome app's request to the WM core. The core authorizes each before acting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Intent {
    /// "What surfaces are in this session?" — the dock/bar/overview need the list
    /// to draw it. Read-only: the core answers from its Fresco enumerate.
    ListSurfaces,
    /// "Focus this surface" — e.g. the overview switcher on a click, or the dock on
    /// an app-icon activate. The core makes it the focused surface (which also
    /// drives input routing + the deadline lane + GPU power, §2.5) and re-declares
    /// the layout to Fresco.
    Focus { surface_id: u32 },
}

/// The core's answer to an `Intent`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Reply {
    /// The session's surfaces + the currently-focused one (answer to `ListSurfaces`).
    Surfaces { surfaces: Vec<WmSurfaceInfo>, focus: u32 },
    /// The intent was carried out (answer to `Focus`).
    Ack,
    /// The intent failed or was refused.
    Err { message: String },
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(v)
}

/// Decode a message from the wire.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_round_trip() {
        for i in [Intent::ListSurfaces, Intent::Focus { surface_id: 7 }] {
            let b = encode(&i).unwrap();
            assert_eq!(decode::<Intent>(&b).unwrap(), i);
        }
    }

    #[test]
    fn replies_round_trip() {
        let r = Reply::Surfaces { surfaces: Vec::new(), focus: 3 };
        assert_eq!(decode::<Reply>(&encode(&r).unwrap()).unwrap(), r);
        assert_eq!(decode::<Reply>(&encode(&Reply::Ack).unwrap()).unwrap(), Reply::Ack);
    }
}
