//! Sigillum's CCID transport (sigillum.md §6.2), for one reader in one
//! driver jail. Every byte from the device is untrusted: each parser is
//! bounded, and a reader that breaks the protocol ends the session.
//!
//! - [`desc`]: the CCID class functional descriptor (what the reader says
//!   it can do).
//! - [`msg`]: `PC_to_RDR_*` / `RDR_to_PC_*` bulk messages.
//! - [`atr`]: the card's Answer To Reset.
//! - [`apdu`]: ISO 7816-4 command and response APDUs.
//! - [`reader`]: a session over a [`reader::Bulk`] pipe: power on, exchange
//!   APDUs (short or extended APDU level), `GET RESPONSE` chaining.
//!
//! The USB side (ugen bulk endpoints) is the driver binary's; tests run
//! over an in-memory card.

pub mod apdu;
pub mod atr;
pub mod desc;
pub mod msg;
pub mod reader;

/// Why a reader session failed. Card status words are not errors here:
/// they come back in [`apdu::Response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Malformed bytes from the reader or card.
    Protocol(&'static str),
    /// The reader can't do what this transport needs (TPDU-level T=1, a
    /// PIN pad only, too small a message).
    Unsupported(&'static str),
    /// No card in the slot, or it was removed.
    NoCard,
    /// The reader reported an error for the slot (bError).
    Slot(u8),
    /// The pipe failed or timed out.
    Io,
    /// A request too big for the reader or for an APDU.
    TooLong,
}
