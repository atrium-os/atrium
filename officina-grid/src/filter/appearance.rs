//! Predicates over the style and conditional formatting Sheets displays.

use officina_core::wire::{Reader, WireError, Writer};

use crate::{addr::CellAddr, book::Workbook, condfmt::ICON_SETS, style::PatternFill};

/// The selected fill or font color. None denotes No Fill or Automatic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorFilter {
    pub cell: bool,
    pub rgb: Option<u32>,
}

/// The selected icon, or the absence of an icon. The set is retained for Excel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IconFilter {
    set: u8,
    index: Option<u8>,
}

impl IconFilter {
    pub fn new(set: u8, index: Option<u8>) -> Option<Self> {
        let size = ICON_SETS.get(set as usize)?.1;
        index
            .is_none_or(|i| i < size)
            .then_some(Self { set, index })
    }

    pub fn selection(self) -> (u8, Option<u8>) {
        (self.set, self.index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppearanceFilter {
    Color(ColorFilter),
    Pattern(PatternFill),
    Icon(IconFilter),
}

impl AppearanceFilter {
    pub(super) fn keeps(self, wb: &Workbook, addr: CellAddr) -> bool {
        match self {
            Self::Pattern(expected) => shown_fill(wb, addr).1 == Some(expected),
            Self::Color(ColorFilter { cell, rgb }) => shown_color(wb, addr, cell) == Some(rgb),
            Self::Icon(icon) => wb.look(addr).icon == icon.index.map(|index| (icon.set, index)),
        }
    }

    pub(super) fn encode(self, w: &mut Writer) {
        match self {
            Self::Color(color) => {
                w.u8(u8::from(!color.cell));
                w.u32(color.rgb.unwrap_or(u32::MAX));
            }
            Self::Pattern(pattern) => {
                w.u8(3);
                pattern.encode(w);
            }
            Self::Icon(icon) => {
                w.u8(2);
                w.u8(icon.set);
                w.u8(icon.index.unwrap_or(u8::MAX));
            }
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        match r.u8()? {
            tag @ (0 | 1) => {
                let rgb = match r.u32()? {
                    u32::MAX => None,
                    n if n <= 0xFF_FFFF => Some(n),
                    _ => return Err(WireError::Invalid("filter color")),
                };
                Ok(Self::Color(ColorFilter {
                    cell: tag == 0,
                    rgb,
                }))
            }
            2 => {
                let set = r.u8()?;
                let index = r.u8()?;
                let icon = IconFilter::new(set, (index != u8::MAX).then_some(index))
                    .ok_or(WireError::Invalid("filter icon"))?;
                Ok(Self::Icon(icon))
            }
            3 => Ok(Self::Pattern(PatternFill::decode(r)?)),
            _ => Err(WireError::Invalid("appearance filter type")),
        }
    }
}

/// The fill and pattern a cell shows: a colour scale's, a conditional
/// format's, or its own.
fn shown_fill(wb: &Workbook, addr: CellAddr) -> (Option<u32>, Option<PatternFill>) {
    let look = wb.look(addr);
    if let Some(scale) = look.scale {
        (Some(scale), None)
    } else if look.dxf.fill.is_some() || look.dxf.pattern.is_some() {
        (look.dxf.fill, look.dxf.pattern)
    } else {
        let style = wb.style(addr);
        (style.fill, style.pattern)
    }
}

/// The colour a cell shows, as a colour filter matches it: its solid fill
/// (`cell`; None inside: no fill), or its text colour (None inside:
/// automatic). None when the fill is a pattern, which no colour matches.
pub fn shown_color(wb: &Workbook, addr: CellAddr, cell: bool) -> Option<Option<u32>> {
    if cell {
        let (fill, pattern) = shown_fill(wb, addr);
        pattern.is_none().then_some(fill)
    } else {
        Some(wb.look(addr).dxf.color.or_else(|| wb.style(addr).font.color))
    }
}
