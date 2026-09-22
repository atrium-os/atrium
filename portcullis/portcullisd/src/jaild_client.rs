//! The jaild client now lives in jaild's own crate (`jaild::client`) so that
//! portcullis-oneshot, which portcullisd depends on, can use the same one.
//! Re-exported here so existing `portcullisd::jaild_client` users are unchanged.

pub use jaild::client::*;
