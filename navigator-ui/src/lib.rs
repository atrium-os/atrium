//! navigator-ui: the Navigator's window (docs/spec/navigator-ui.md).
//!
//! Build step 1 (§14): the static shell. The rail with its trails, the
//! toolbar with the tier chip and its light, the prompt row, the optional
//! status bar and Start, drawn by Pergola from a stand-in trail store
//! ([`fake`]), with the page as a placeholder slot ([`shell::PAGE_SLOT`])
//! where Limen binds the jailed page from step 2. The origin strip above
//! the window is frescod's, never drawn here (§6.1).
//!
//! Step 2 ([`live`]): with `NAVIGATORD_SOCKET` set, pages come from
//! navigatord and the page slot holds a doc-viewer child (Limen L6).

pub mod clock;
pub mod disk;
pub mod fake;
pub mod keys;
pub mod live;
pub mod shell;
pub mod trail;
pub mod ui;
pub mod where_field;
