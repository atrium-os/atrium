//! Widget vocabulary — the composable primitives apps assemble.
//!
//! Each widget is a `View`. Apps construct widgets by value, hold
//! them in their own structures, and let the toolkit's `App::tick`
//! drive them. Widget code reads its own state via `Mutable<T>` (sync)
//! and emits nodes via `Ctx`.
//!
//! Phase 5 ships:
//!   - `Button` — clickable rect with optional text label
//!   - `TextField` — editable single-line text
//!
//! Both reference theme tokens exclusively (no raw colors, sizes, or
//! radii) per `docs/design/atrium-visual-language.md` §10.

pub mod basics;
pub mod button;
pub mod chip;
pub mod label;
pub mod phosphor;
pub mod text_field;

pub use basics::{Avatar, Divider, Dot, ProgressBar};
pub use button::Button;
pub use chip::{Chip, ListRow};
pub use label::{Glyph, Label};
pub use text_field::TextField;
