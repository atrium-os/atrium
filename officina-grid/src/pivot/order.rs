//! How visible members are ordered within each parent on a pivot axis.

use super::{Acc, MAX_ITEMS, PivotField, get_value, key, put_value};
use crate::value::Value;
use officina_core::wire::{Reader, WireError, Writer};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq)]
pub enum AxisSort {
    #[default]
    Default,
    Manual(Vec<Value>),
    Source,
    Name {
        descending: bool,
    },
    Data {
        caption: Arc<str>,
        descending: bool,
    },
}

impl AxisSort {
    pub(super) fn valid(&self) -> bool {
        match self {
            Self::Manual(items) => items.len() <= MAX_ITEMS,
            Self::Data { caption, .. } => {
                !caption.is_empty() && caption.len() <= crate::strings::MAX_TEXT
            }
            _ => true,
        }
    }

    pub(super) fn encode(&self, w: &mut Writer) {
        match self {
            Self::Default => w.u8(0),
            Self::Manual(items) => {
                w.u8(1);
                w.var(items.len() as u64);
                for item in items {
                    put_value(w, item);
                }
            }
            Self::Source => w.u8(2),
            Self::Name { descending } => w.u8(3 + u8::from(*descending)),
            Self::Data {
                caption,
                descending,
            } => {
                w.u8(5 + u8::from(*descending));
                w.str(caption);
            }
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            0 => Self::Default,
            1 => {
                let n = r.count("ordered pivot items", MAX_ITEMS as u64, 1)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(get_value(r)?);
                }
                Self::Manual(items)
            }
            2 => Self::Source,
            tag @ (3 | 4) => Self::Name {
                descending: tag == 4,
            },
            tag @ (5 | 6) => {
                let caption =
                    Arc::from(r.str("pivot sort value field", crate::strings::MAX_TEXT as u64)?);
                Self::Data {
                    caption,
                    descending: tag == 6,
                }
            }
            _ => return Err(WireError::Invalid("pivot item sort")),
        })
    }
}

type Path = Vec<u32>;
type Positions = HashMap<Path, HashMap<u32, u32>>;

/// Data sorting can give different orders to the same members under different parents.
pub(super) struct Order {
    levels: Vec<Option<Positions>>,
}

impl Order {
    pub fn new(
        axis: &[PivotField],
        column: bool,
        paths: &[Path],
        acc: &HashMap<(Path, Path), Vec<Acc>>,
        value_field: impl Fn(&str) -> Option<(usize, super::Agg)>,
    ) -> Self {
        let levels = axis
            .iter()
            .enumerate()
            .map(|(level, field)| {
                let AxisSort::Data {
                    caption,
                    descending,
                } = &field.sort
                else {
                    return None;
                };
                let (data, agg) = value_field(caption)?;
                let mut families: HashMap<Path, Vec<(u32, Option<f64>)>> = HashMap::new();
                for path in paths {
                    let item = path[level];
                    let siblings = families.entry(path[..level].to_vec()).or_default();
                    // Paths are visited in item-ID order, so repeats of a child are adjacent.
                    if siblings.last().is_some_and(|(last, _)| *last == item) {
                        continue;
                    }
                    let members = path[..=level].to_vec();
                    let target = if column {
                        (Vec::new(), members)
                    } else {
                        (members, Vec::new())
                    };
                    let amount = match acc[&target][data].result(agg) {
                        Value::Num(n) if n.is_finite() => Some(n),
                        _ => None,
                    };
                    siblings.push((item, amount));
                }
                Some(
                    families
                        .into_iter()
                        .map(|(parent, mut siblings)| {
                            siblings.sort_by(|(a, left), (b, right)| {
                                match (left, right) {
                                    (Some(left), Some(right)) => {
                                        if *descending {
                                            right.total_cmp(left)
                                        } else {
                                            left.total_cmp(right)
                                        }
                                    }
                                    (Some(_), None) => std::cmp::Ordering::Less,
                                    (None, Some(_)) => std::cmp::Ordering::Greater,
                                    _ => std::cmp::Ordering::Equal,
                                }
                                .then_with(|| a.cmp(b))
                            });
                            let positions = siblings
                                .into_iter()
                                .enumerate()
                                .map(|(i, (id, _))| (id, i as u32))
                                .collect();
                            (parent, positions)
                        })
                        .collect(),
                )
            })
            .collect();
        Self { levels }
    }

    pub fn key(&self, parent: &[u32], item: u32) -> (u32, u32) {
        let position = self
            .levels
            .get(parent.len())
            .and_then(Option::as_ref)
            .and_then(|parents| parents.get(parent))
            .and_then(|positions| positions.get(&item))
            .copied()
            .unwrap_or(item);
        (position, item)
    }

    pub fn sort(&self, paths: &mut [Path]) {
        if self.levels.iter().all(Option::is_none) {
            return;
        }
        paths.sort_unstable_by(|a, b| match a.iter().zip(b).position(|(a, b)| a != b) {
            Some(i) => self.key(&a[..i], a[i]).cmp(&self.key(&a[..i], b[i])),
            None => a.len().cmp(&b.len()),
        });
    }
}

/// Missing explicitly ordered members still need entries in a foreign definition.
pub(super) fn add_missing(items: &mut Vec<Value>, prior: &[Value]) {
    let mut seen: std::collections::HashSet<_> = items.iter().map(key).collect();
    items.extend(prior.iter().filter(|item| seen.insert(key(item))).cloned());
}
