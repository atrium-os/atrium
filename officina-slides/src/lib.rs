//! Officina's presentation engine (docs/spec/officina.md §9, §16.33).
//!
//! - [`model`]: a [`model::Deck`] is a slide size, a theme, masters, layouts
//!   and slides; a slide is a tree of shapes, text frames holding small
//!   `officina-text` documents.
//! - [`resolve`]: what a shape shows once its placeholder's layout and master
//!   fill in what it leaves unset, theme colours and fonts resolved.
//! - [`ops`]: every change is a [`ops::DeckOp`] with an exact inverse.
//! - [`native`]: the `.oslides` file, one chunk per slide and per media piece,
//!   so opening reads only what the first slide needs.
//! - [`draw`]: a slide as a display list in EMU.
//!
//! Geometry is in EMU (914,400 to the inch), which `.pptx` and `.odp` both
//! say exactly.

pub mod draw;
pub mod geom;
pub mod model;
pub mod native;
pub mod ops;
pub mod resolve;

/// EMU to the inch, point and centimetre.
pub const EMU_PER_INCH: i64 = 914_400;
pub const EMU_PER_PT: i64 = 12_700;
pub const EMU_PER_CM: i64 = 360_000;

/// EMU to `officina-text` layout units, rounded.
pub fn emu_to_lu(e: i64) -> i64 {
    let n = e * officina_text::LU;
    if n >= 0 {
        (n + EMU_PER_PT / 2) / EMU_PER_PT
    } else {
        (n - EMU_PER_PT / 2) / EMU_PER_PT
    }
}

/// `officina-text` layout units to EMU.
pub fn lu_to_emu(l: i64) -> i64 {
    l * EMU_PER_PT / officina_text::LU
}
