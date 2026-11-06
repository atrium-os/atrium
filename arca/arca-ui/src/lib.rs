//! arca-ui: Arca's trusted surfaces. For now, the credential field that
//! Limen embeds in an app's window (arca-autofill.md §3, §5, §6, §8).
//!
//! The field is Arca's window, pinned into the app's as a `credential`
//! slot, so the app never sees its pixels, its keys or its value. A fill
//! takes one press on Arca's chip, which frescod vouches was seen (purpose
//! FILL), and gives the app only a handle that the courier redeems after
//! TLS to the item's recipient.

pub mod arca;
pub mod field;
pub mod gate;
pub mod keymap;
pub mod view;
