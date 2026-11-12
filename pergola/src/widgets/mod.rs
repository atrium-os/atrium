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
pub mod canvas;
pub mod chip;
pub mod choice;
pub mod credential_field;
pub mod group;
pub mod label;
pub mod phosphor;
pub mod place;
pub mod popup;
pub mod scroll_view;
pub mod slider;
pub mod text_field;
pub mod virtual_list;

pub use basics::{Avatar, Divider, Dot, ProgressBar};
pub use button::Button;
pub use canvas::{Canvas, CanvasEvent, Item as CanvasItem};
pub use chip::{Chip, ListRow};
pub use choice::{Check, Checkbox, RadioGroup, Segmented, Tabs, Toggle};
pub use credential_field::{CredentialField, CredentialState};
pub use group::{GroupBox, Section};
pub use label::{Glyph, Label};
pub use popup::{Dialog, DialogButton, Dropdown, Menu, MenuButton, MenuItem, MenuState, TriggerIcon};
pub use scroll_view::{ScrollState, ScrollView};
pub use slider::Slider;
pub use text_field::TextField;
pub use virtual_list::VirtualList;
