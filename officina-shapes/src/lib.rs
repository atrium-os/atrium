//! Shape outlines shared by Officina's documents and decks: paths in a
//! shape's own box, OOXML's 187 preset shapes evaluated from the standard's
//! definitions (ECMA-376 Part 1 §20.1.9; data as Apache POI ships it,
//! Apache-2.0) and custom geometry with guides. No I/O, no dependencies.

use std::sync::Arc;

pub mod geom;
pub mod presets;

/// A path step in a custom shape's own coordinates (`0..w`, `0..h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathCmd {
    Move(i64, i64),
    Line(i64, i64),
    Cubic(i64, i64, i64, i64, i64, i64),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Path {
    pub w: i64,
    pub h: i64,
    pub cmds: Vec<PathCmd>,
    pub filled: bool,
    pub stroked: bool,
}

/// A shape's outline: an OOXML preset by name (`rect`, `roundRect`,
/// `ellipse`, ...) with its adjust values, or paths of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Geom {
    Preset { name: Arc<str>, adj: Vec<(Arc<str>, i64)> },
    Custom(Vec<Path>),
}

impl Geom {
    pub fn preset(name: &str) -> Geom {
        Geom::Preset { name: name.into(), adj: Vec::new() }
    }
}

