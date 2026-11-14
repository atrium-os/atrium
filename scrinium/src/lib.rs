//! Scrinium, Atrium's file manager and system file picker
//! ([docs/spec/scrinium.md]).
//!
//! - [`fs`]: listing and changing files under a place's root, never
//!   leaving it and never following a link.
//! - [`browser`]: what is shown and what the user did, with no drawing.
//! - [`view`]: the window, drawn with Pergola: the file manager, or the
//!   picker in an app's slot.
//! - [`places`]: where the places come from (the sticks dispensatord gives
//!   the file manager, and folders it was given).

pub mod browser;
pub mod fs;
pub mod places;
pub mod view;
