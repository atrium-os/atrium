//! The `credential` role (arca-autofill.md §8): a credential field drawn and
//! typed into by Arca, embedded in the app's window. No message carries the
//! secret: the parent learns that the field is filled, the username (unless
//! the item keeps it private), and a handle to submit through a courier.
use serde::{Deserialize, Serialize};

/// What the field is for (arca-autofill.md §2). A hint: it picks which
/// items Arca offers, never the recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Purpose {
    CurrentPassword,
    NewPassword,
    OneTimeCode,
    /// A username the item marks private: a slot too, not the app's field.
    PrivateUsername,
}

/// One submit's worth of reference to the value, meaningful only on this
/// app's Arca channel. Expires when the slot detaches or after 10 minutes.
pub type Handle = [u8; 16];

pub const HANDLE_TTL_SECS: u64 = 600;

/// Parent to child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToChild {
    /// `username_field`: the parent has its own username field that Arca
    /// may fill on the same press.
    Configure { purpose: Purpose, username_field: bool },
}

impl ToChild {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

/// Child to parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToParent {
    /// Filled on a press on Arca's chip. `username` goes into the
    /// parent's own field; `None` when it has none or the item keeps it
    /// private.
    Filled { username: Option<String> },
    /// The handle for this fill or for what the user typed.
    Handle { handle: Handle },
    /// The field was emptied; any handle is void.
    Cleared,
}

/// The longest username Arca hands a parent.
pub const USERNAME_MAX: usize = 256;

impl ToParent {
    /// Decode a child's message, refusing anything out of shape: a
    /// parent never trusts the child's bytes more than its own input.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let m: Self = postcard::from_bytes(b).ok()?;
        if let ToParent::Filled { username: Some(u) } = &m {
            if u.len() > USERNAME_MAX || u.chars().any(char::is_control) {
                return None;
            }
        }
        Some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("encodes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_refusals() {
        for m in [ToParent::Filled { username: Some("ana".into()) }, ToParent::Handle { handle: [9; 16] }, ToParent::Cleared] {
            assert_eq!(ToParent::decode(&m.encode()), Some(m));
        }
        let long = ToParent::Filled { username: Some("a".repeat(USERNAME_MAX + 1)) };
        assert_eq!(ToParent::decode(&long.encode()), None);
        let ctl = ToParent::Filled { username: Some("a\u{1b}[2J".into()) };
        assert_eq!(ToParent::decode(&ctl.encode()), None);
        assert_eq!(ToParent::decode(&[0xff, 0xff]), None);
    }
}
