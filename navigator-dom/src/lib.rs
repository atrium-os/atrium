//! The arena DOM and the HTML parser, shared.
//!
//! ★★ EXTRACTED SO THERE IS EXACTLY ONE OF EACH. The converter parses a
//! document; the backend re-parses the document the converter serialized into
//! a recording. If those were two parsers they would eventually disagree
//! about the same bytes — and a disagreement between the thing that produced
//! an artifact and the thing that renders it is not a bug you find by
//! testing either one.
//!
//! This is the same argument the converter already applies internally: its
//! `innerHTML` setter goes through the real parser rather than a second
//! hand-rolled one, for exactly this reason. Extracting it makes the rule
//! hold across crates rather than only within one.
//!
//! ★ THE PROFILE IS HERE TOO, AND I SAID IT WOULD NOT BE.
//!
//! When this crate was split out it carried a line claiming it "deliberately
//! contains no policy: no ceilings, no profile". That was wrong, and the
//! distinction it missed is worth keeping:
//!
//!   - The Document Profile's ceilings are a CONTRACT. They are normative in
//!     the spec, and both sides must read the same numbers from the same
//!     place — a converter that emits a document its renderer will refuse has
//!     produced nothing. That is the parser argument again, applied to a
//!     different shared fact, so `profile` lives here.
//!   - What to DO about a document outside the profile is policy, and that
//!     stays with each side. The converter reports violations while it still
//!     holds the document and can say which one; the backend refuses to
//!     render it. Neither decision belongs in this crate.
//!
//! So the rule is not "no policy" but "no DECISIONS": shared facts here,
//! shared measurements here, and every choice made by the crate that has to
//! live with it.

pub mod dom;
pub mod parse;
pub mod profile;

pub use dom::{Dom, Handle, Kind, Node};
pub use parse::{parse, parse_fragment};
