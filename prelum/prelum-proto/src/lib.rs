//! Prelum's messages (printing.md §6): prelumd ⇄ its workers ([`ipp`],
//! [`render`], framed by [`frame`]) and prelumd ⇄ its clients ([`print`],
//! Aqueduct `CLASS_PRINT`).

#![forbid(unsafe_code)]

pub mod frame;
pub mod ipp;
pub mod print;
pub mod render;

pub use prelum_ipp_codec as codec;
