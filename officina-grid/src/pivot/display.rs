//! Display calculations operate on the aggregates of a single value field.

use super::{Acc, Key, Order, get_value, key, put_value};
use crate::value::{ErrorKind, Value};
use officina_core::wire::{Reader, WireError, Writer};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum BaseItem {
    Named(Value),
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceCalc {
    Difference,
    Percent,
    PercentDifference,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum ShowAs {
    #[default]
    Normal,
    PercentOfRow,
    PercentOfColumn,
    PercentOfTotal,
    Index,
    Reference {
        calc: ReferenceCalc,
        field: u32,
        item: BaseItem,
    },
    RunningTotal(u32),
    PercentOfParentRow,
    PercentOfParentColumn,
    PercentOfParent(u32),
    PercentOfRunningTotal(u32),
    Rank {
        field: u32,
        descending: bool,
    },
}

impl ShowAs {
    pub fn base_field(&self) -> Option<u32> {
        match self {
            Self::Reference { field, .. }
            | Self::RunningTotal(field)
            | Self::PercentOfParent(field)
            | Self::PercentOfRunningTotal(field)
            | Self::Rank { field, .. } => Some(*field),
            _ => None,
        }
    }

    pub fn is_excel_extension(&self) -> bool {
        matches!(
            self,
            Self::PercentOfParentRow
                | Self::PercentOfParentColumn
                | Self::PercentOfParent(_)
                | Self::PercentOfRunningTotal(_)
                | Self::Rank { .. }
        )
    }

    pub fn is_percent(&self) -> bool {
        matches!(
            self,
            Self::PercentOfRow
                | Self::PercentOfColumn
                | Self::PercentOfTotal
                | Self::PercentOfParentRow
                | Self::PercentOfParentColumn
                | Self::PercentOfParent(_)
                | Self::PercentOfRunningTotal(_)
                | Self::Reference {
                    calc: ReferenceCalc::Percent | ReferenceCalc::PercentDifference,
                    ..
                }
        )
    }

    pub(super) fn encode(&self, w: &mut Writer) {
        let tag = match self {
            Self::Normal => 0,
            Self::PercentOfRow => 1,
            Self::PercentOfColumn => 2,
            Self::PercentOfTotal => 3,
            Self::Index => 4,
            Self::Reference { calc, .. } => 5 + *calc as u8,
            Self::RunningTotal(_) => 8,
            Self::PercentOfParentRow => 9,
            Self::PercentOfParentColumn => 10,
            Self::PercentOfParent(_) => 11,
            Self::PercentOfRunningTotal(_) => 12,
            Self::Rank { descending, .. } => 13 + u8::from(*descending),
        };
        w.u8(tag);
        if let Some(field) = self.base_field() {
            w.var(u64::from(field));
        }
        if let Self::Reference { item, .. } = self {
            match item {
                BaseItem::Named(value) => {
                    w.u8(0);
                    put_value(w, value);
                }
                BaseItem::Previous => w.u8(1),
                BaseItem::Next => w.u8(2),
            }
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            0 => Self::Normal,
            1 => Self::PercentOfRow,
            2 => Self::PercentOfColumn,
            3 => Self::PercentOfTotal,
            4 => Self::Index,
            tag @ 5..=7 => {
                let field = super::get_field(r)?;
                let item = match r.u8()? {
                    0 => BaseItem::Named(get_value(r)?),
                    1 => BaseItem::Previous,
                    2 => BaseItem::Next,
                    _ => return Err(WireError::Invalid("pivot reference item")),
                };
                let calc = [
                    ReferenceCalc::Difference,
                    ReferenceCalc::Percent,
                    ReferenceCalc::PercentDifference,
                ][(tag - 5) as usize];
                Self::Reference { calc, field, item }
            }
            8 => Self::RunningTotal(super::get_field(r)?),
            9 => Self::PercentOfParentRow,
            10 => Self::PercentOfParentColumn,
            11 => Self::PercentOfParent(super::get_field(r)?),
            12 => Self::PercentOfRunningTotal(super::get_field(r)?),
            tag @ 13..=14 => Self::Rank {
                field: super::get_field(r)?,
                descending: tag == 14,
            },
            _ => return Err(WireError::Invalid("pivot display calculation")),
        })
    }
}

type Path = (Vec<u32>, Vec<u32>);

#[derive(Clone, Copy)]
pub(super) struct Slot {
    pub column: bool,
    pub level: usize,
}

impl Slot {
    fn path<'a>(self, row: &'a [u32], col: &'a [u32]) -> &'a [u32] {
        if self.column { col } else { row }
    }

    fn path_mut(self, paths: &mut Path) -> &mut Vec<u32> {
        if self.column {
            &mut paths.1
        } else {
            &mut paths.0
        }
    }

    fn key(self, row: &[u32], col: &[u32]) -> Option<(Path, u32)> {
        let item = *self.path(row, col).get(self.level)?;
        let mut paths = (row.to_vec(), col.to_vec());
        self.path_mut(&mut paths)[self.level] = 0;
        Some((paths, item))
    }
}

pub(super) struct Display<'a> {
    mode: &'a ShowAs,
    slot: Option<Slot>,
    named: Option<u32>,
    order: &'a Order,
    peers: HashMap<Path, Vec<u32>>,
    series: HashMap<Path, Vec<Value>>,
}

impl<'a> Display<'a> {
    pub fn new(
        mode: &'a ShowAs,
        slot: Option<Slot>,
        items: Option<&HashMap<Key, u32>>,
        acc: &HashMap<Path, Vec<Acc>>,
        field: usize,
        agg: super::Agg,
        order: &'a Order,
    ) -> Self {
        let named = match mode {
            ShowAs::Reference {
                item: BaseItem::Named(value),
                ..
            } => items.and_then(|items| items.get(&key(value)).copied()),
            _ => None,
        };
        let mut peers: HashMap<Path, Vec<u32>> = HashMap::new();
        let mut series = HashMap::new();
        if let (
            Some(slot),
            ShowAs::RunningTotal(_)
            | ShowAs::PercentOfRunningTotal(_)
            | ShowAs::Rank { .. }
            | ShowAs::Reference {
                item: BaseItem::Previous | BaseItem::Next,
                ..
            },
        ) = (slot, mode)
        {
            for (row, col) in acc.keys() {
                if let Some((paths, item)) = slot.key(row, col) {
                    peers.entry(paths).or_default().push(item);
                }
            }
            for (paths, siblings) in &mut peers {
                let parent = &slot.path(&paths.0, &paths.1)[..slot.level];
                siblings.sort_unstable_by_key(|&item| order.key(parent, item));
                if matches!(mode, ShowAs::Reference { .. }) {
                    continue;
                }
                let mut target = paths.clone();
                let mut raw: Vec<_> = siblings
                    .iter()
                    .map(|&item| {
                        slot.path_mut(&mut target)[slot.level] = item;
                        acc[&target][field].result(agg)
                    })
                    .collect();
                if let ShowAs::Rank { descending, .. } = mode {
                    rank(&mut raw, *descending);
                } else {
                    // Keep a separate sum for each group of visible peers.
                    let mut sum = 0.0;
                    let mut error = None;
                    for value in &mut raw {
                        match *value {
                            Value::Num(n) => sum += n,
                            Value::Err(e) => {
                                error.get_or_insert(e);
                            }
                            _ => {}
                        }
                        *value = error.map_or(Value::Num(sum), Value::Err);
                    }
                }
                series.insert(paths.clone(), raw);
            }
        }
        Self {
            mode,
            slot,
            named,
            peers,
            series,
            order,
        }
    }

    fn series_value(&self, row: &[u32], col: &[u32]) -> Option<(&[Value], usize)> {
        let (paths, item) = self.slot?.key(row, col)?;
        let siblings = self.peers.get(&paths)?;
        let index = self.peer_position(self.slot?, &paths, siblings, item)?;
        Some((self.series.get(&paths)?, index))
    }

    fn peer_position(
        &self,
        slot: Slot,
        paths: &Path,
        siblings: &[u32],
        item: u32,
    ) -> Option<usize> {
        let parent = &slot.path(&paths.0, &paths.1)[..slot.level];
        let target = self.order.key(parent, item);
        siblings
            .binary_search_by_key(&target, |&id| self.order.key(parent, id))
            .ok()
    }

    pub fn value(&self, row: &[u32], col: &[u32], raw: impl Fn(&[u32], &[u32]) -> Value) -> Value {
        let value = raw(row, col);
        if !matches!(value, Value::Num(_)) {
            return value;
        }
        match self.mode {
            ShowAs::Normal => value,
            ShowAs::PercentOfRow => ratio(value, raw(row, &[])),
            ShowAs::PercentOfColumn => ratio(value, raw(&[], col)),
            ShowAs::PercentOfTotal => ratio(value, raw(&[], &[])),
            ShowAs::PercentOfParentRow => {
                ratio(value, raw(&row[..row.len().saturating_sub(1)], col))
            }
            ShowAs::PercentOfParentColumn => {
                ratio(value, raw(row, &col[..col.len().saturating_sub(1)]))
            }
            ShowAs::PercentOfParent(_) => {
                let Some(slot) = self.slot else {
                    return Value::Empty;
                };
                let total = if slot.column {
                    raw(row, &col[..col.len().min(slot.level)])
                } else {
                    raw(&row[..row.len().min(slot.level)], col)
                };
                ratio(value, total)
            }
            ShowAs::Index => {
                let product = numeric(value, raw(&[], &[]), |a, b| a * b);
                let totals = numeric(raw(row, &[]), raw(&[], col), |a, b| a * b);
                ratio(product, totals)
            }
            ShowAs::Reference { calc, item, .. } => {
                let Some(slot) = self.slot else {
                    return Value::Empty;
                };
                let Some((mut paths, current)) = slot.key(row, col) else {
                    return Value::Empty;
                };
                let base = match item {
                    BaseItem::Named(_) => self.named,
                    BaseItem::Previous | BaseItem::Next => {
                        self.peers.get(&paths).and_then(|peers| {
                            let index = self.peer_position(slot, &paths, peers, current)?;
                            let next = if matches!(item, BaseItem::Next) {
                                index.checked_add(1)
                            } else {
                                index.checked_sub(1)
                            }?;
                            peers.get(next).copied()
                        })
                    }
                };
                let Some(base) = base else {
                    return Value::Empty;
                };
                if current == base && *calc != ReferenceCalc::Percent {
                    return Value::Empty;
                }
                slot.path_mut(&mut paths)[slot.level] = base;
                let reference = raw(&paths.0, &paths.1);
                match calc {
                    ReferenceCalc::Difference => numeric(value, reference, |a, b| a - b),
                    ReferenceCalc::Percent => ratio(value, reference),
                    ReferenceCalc::PercentDifference => {
                        let delta = numeric(value, reference.clone(), |a, b| a - b);
                        ratio(delta, reference)
                    }
                }
            }
            ShowAs::RunningTotal(_) | ShowAs::Rank { .. } => self
                .series_value(row, col)
                .map_or(Value::Empty, |(series, index)| series[index].clone()),
            ShowAs::PercentOfRunningTotal(_) => self
                .series_value(row, col)
                .map_or(Value::Empty, |(series, index)| {
                    ratio(series[index].clone(), series.last().unwrap().clone())
                }),
        }
    }
}

fn rank(values: &mut [Value], descending: bool) {
    let mut sorted: Vec<_> = values
        .iter()
        .enumerate()
        .filter_map(|(i, value)| match value {
            Value::Num(n) if n.is_finite() => Some((i, *n)),
            _ => None,
        })
        .collect();
    sorted.sort_unstable_by(|(_, a), (_, b)| {
        if descending {
            b.total_cmp(a)
        } else {
            a.total_cmp(b)
        }
    });
    let mut previous = None;
    let mut rank = 1;
    for (position, (i, number)) in sorted.into_iter().enumerate() {
        if previous != Some(number) {
            rank = position + 1;
            previous = Some(number);
        }
        values[i] = Value::Num(rank as f64);
    }
}

fn numeric(a: Value, b: Value, f: impl FnOnce(f64, f64) -> f64) -> Value {
    match (a, b) {
        (Value::Num(a), Value::Num(b)) => Value::Num(f(a, b)),
        (Value::Err(e), _) | (_, Value::Err(e)) => Value::Err(e),
        _ => Value::Empty,
    }
}

fn ratio(a: Value, b: Value) -> Value {
    if matches!((&a, &b), (Value::Num(_), Value::Num(n)) if *n == 0.0) {
        return Value::Err(ErrorKind::Div0);
    }
    numeric(a, b, |a, b| a / b)
}
