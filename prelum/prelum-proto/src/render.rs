//! `prelum-render` → prelumd (printing.md §6.1). The worker's stdin is the
//! job's PDF, its settings are its arguments; its stdout carries these.

use prelum_ipp_codec::ops::Format;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ev {
    /// The document opened: this many sheets, in this format (a PDF that
    /// can't be rewritten comes as the fallback raster format).
    Started { sheets: u32, format: Format },
    /// The next bytes of what goes to the printer.
    Data(Vec<u8>),
    /// Sheet `n` (from 1) is fully written.
    Sheet(u32),
    /// Everything is written.
    Done { sheets: u32, bytes: u64 },
    /// The job can't be prepared (plain text for the person).
    Failed(String),
    /// `--format preview`: the document makes this many sheets; the
    /// [`Ev::Preview`]s asked for follow, then [`Ev::Done`].
    Sheets(u32),
    /// One sheet's preview (printing.md §5.2): 8-bit sRGB, `w` x `h`.
    Preview { sheet: u32, w: u32, h: u32, rgb: Vec<u8> },
}
