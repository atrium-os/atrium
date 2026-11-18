//! `DocumentView` (pergola.md §10.2): a document drawn by the document
//! engine and scrolled by Pergola's `ScrollView`.
//!
//! The engine is a band source (pergola.md §10.4, `navigator_render::source`):
//! the view says which range it wants at which zoom; the source lays out
//! what it needs, writes one scene per band (and an overlay for what paints
//! across bands) into the view's sink, evicts what is far away, and says
//! what changed. Here the sink turns each scene into Pergola nodes once
//! ([`convert`]), and every render places the resident bands, keyed by
//! band, in a free stack as tall as the document, so a band keeps its node
//! ids while it stays resident and the scroll range is the whole document.
//! Ranges no band covers yet show placeholders, never a blank.
//!
//! While the document's height is still an estimate, each render lays out
//! two more bands in the background and asks for another render, until it
//! settles. Anchoring keeps the reader's place when a band above the view
//! changes height.
//!
//! For an app's own trusted text. A document the app doesn't trust goes to
//! a jailed doc-viewer child instead (limen.md §2.1), which runs this same
//! widget in its own window.

pub mod faces;
pub use faces::Faces;

#[cfg(feature = "view")]
pub mod convert;
#[cfg(feature = "view")]
mod view;
#[cfg(feature = "view")]
pub use view::*;
