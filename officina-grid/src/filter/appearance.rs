//! Predicates over the style and conditional formatting Sheets displays.

use officina_core::wire::{Reader, WireError, Writer};

use crate::{addr::CellAddr, book::Workbook, condfmt::ICON_SETS};

/// The selected fill or font color. None denotes No Fill or Automatic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorFilter {
    pub cell: bool,
    pub rgb: Option<u32>,
}

/// The selected icon, or the absence of an icon. The set is retained for Excel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceFilter {
    Color(ColorFilter),
    Icon(IconFilter),
}

impl AppearanceFilter {
    pub(super) fn keeps(self, wb: &Workbook, addr: CellAddr) -> bool {
        let look = wb.look(addr);
        match self {
            Self::Color(ColorFilter { cell: true, rgb }) => {
                let fill = look.scale.or(look.dxf.fill).or_else(|| wb.style(addr).fill);
                fill == rgb
            }
            Self::Color(ColorFilter { cell: false, rgb }) => {
                let font = look.dxf.color.or_else(|| wb.style(addr).font.color);
                font == rgb
            }
            Self::Icon(icon) => look.icon == icon.index.map(|index| (icon.set, index)),
        }
    }

    pub(super) fn encode(self, w: &mut Writer) {
        match self {
            Self::Color(color) => {
                w.u8(u8::from(!color.cell));
                w.u32(color.rgb.unwrap_or(u32::MAX));
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
            _ => Err(WireError::Invalid("appearance filter type")),
        }
    }
}
