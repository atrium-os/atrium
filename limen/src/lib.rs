//! Limen, the cross-jail embed broker ([docs/spec/limen.md]).
//!
//! - [`catalogue`]: the roles, their providers and slot kinds (§2).
//! - [`proto`]: limend's wire (CLASS_LIMEN).
//! - [`daemon`]: limend itself, over traits for frescod and the launcher so
//!   it can be tested without either.
//! - [`client`]: the parent's and the child's ends.
//! - [`audio`]: choragusd's attribution tickets for embedded players.
//! - [`manifests`]: `embeds` and `[roles] implements` from installed manifests.
//! - [`fdpass`]: the checks on a descriptor a parent passes with a message.
//! - one module per role's messages: [`credential`], [`doc_viewer`], [`media_player`], [`picker`], [`print`],
//!   [`share`] (the share sheet) and [`share_target`].

pub mod audio;
pub mod catalogue;
pub mod client;
pub mod credential;
pub mod daemon;
pub mod doc_viewer;
pub mod fdpass;
pub mod fresco;
pub mod launch;
pub mod manifests;
pub mod media_player;
pub mod pdf_viewer;
pub mod picker;
pub mod print;
pub mod proto;
pub mod share;
pub mod share_target;
pub mod web_island;
pub mod sign_in;
