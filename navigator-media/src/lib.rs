//! ★ THE NAVIGATOR'S MEDIA FRONT END (fresco-video.md P3; limen.md §2.1–2.2;
//! legacy-web §8.1). Playback is native: the Limen `media-player` demuxes,
//! decodes, presents and draws its own controls in its jail and window. The
//! Navigator only says what to play and relays what a page may know.
//!
//! - [`slots`]: the video embed slots a document's bands carry (the NSG
//!   `embed` leaf), as the list the UI places — before Limen L6 as slot nodes
//!   in its own `ScrollView`, after it as the doc-viewer child's `Embeds`.
//! - [`session`]: one live `media-player` embed per started slot: `Load` with
//!   the resolved URL, play on a press or by the autoplay policy, at most
//!   [`session::LIVE_MAX`] live per page, released with the slot.
//! - [`mse`]: `HTMLMediaElement` and `MediaSource` / `SourceBuffer` mapped
//!   onto the role's segment feed, for an engine island (legacy-web §8.1),
//!   engine-independent: an engine binds its JS objects to these.
//! - [`feed`]: a feed the Navigator drives itself from a segment index:
//!   what to fetch after a seek, refilling ahead, evicting far behind.
//! - [`fake`]: a fake `media-player` child that checks the order and limits
//!   of what it is sent. Real playback is the native player's (P1).
pub mod fake;
pub mod feed;
pub mod mse;
pub mod session;
pub mod slots;
pub mod url;
