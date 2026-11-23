//! Source item groupings for pivot axes.

use super::{get_value, key, put_value, Key, MAX_GROUP_MEMBERS, MAX_GROUPS, Pivot};
use crate::Workbook;
use crate::addr::{CellAddr, MAX_COLS, Rect};
use crate::eval::dates::DateSystem;
use crate::value::Value;
use officina_core::wire::{Reader, WireError, Writer};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone, PartialEq)]
pub struct ItemGroup {
    pub name: Arc<str>,
    pub members: Vec<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePart {
    Seconds,
    Minutes,
    Hours,
    Days,
    Months,
    Quarters,
    Years,
}

impl DatePart {
    pub const ALL: [Self; 7] = [
        Self::Seconds,
        Self::Minutes,
        Self::Hours,
        Self::Days,
        Self::Months,
        Self::Quarters,
        Self::Years,
    ];
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupKind {
    Named(Vec<ItemGroup>),
    /// Excel discrete groups map typed source items to typed group values.
    Discrete(Vec<(Value, Value)>),
    /// Fixed Excel numeric bins, with the original labels in cache order.
    NumericBins { start: f64, end: f64, step: f64, items: Vec<Value> },
    Intervals {
        start: Option<f64>,
        end: Option<f64>,
        step: f64,
        dates: bool,
    },
    DateParts {
        start: Option<f64>,
        end: Option<f64>,
        part: DatePart,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupField {
    pub field: u32,
    pub source: u32,
    /// The virtual field's name. An in-place grouping uses the source header.
    pub name: Arc<str>,
    pub kind: GroupKind,
}

impl GroupField {
    pub(super) fn valid(&self, width: u32) -> bool {
        if (self.field < width && self.field != self.source)
            || (self.field >= width
                && !(MAX_COLS..MAX_COLS + MAX_GROUPS as u32).contains(&self.field))
            || self.name.len() > 255
            || (self.field >= MAX_COLS && self.name.is_empty())
        {
            return false;
        }
        let bounds = |start: Option<f64>, end: Option<f64>| {
            start.is_none_or(f64::is_finite)
                && end.is_none_or(f64::is_finite)
                && start.zip(end).is_none_or(|(a, b)| a <= b)
        };
        match &self.kind {
            GroupKind::Named(groups) => {
                groups.len() <= MAX_GROUP_MEMBERS
                    && groups.iter().map(|g| g.members.len()).sum::<usize>() <= MAX_GROUP_MEMBERS
            }
            GroupKind::Discrete(items) => self.field >= MAX_COLS && items.len() <= MAX_GROUP_MEMBERS,
            GroupKind::NumericBins { start, end, step, items } =>
                self.source < width && numeric_bins(*start, *end, *step).is_some_and(|n| n + 2 == items.len()),
            GroupKind::Intervals {
                start, end, step, ..
            } => bounds(*start, *end) && step.is_finite() && *step > 0.0,
            GroupKind::DateParts { start, end, .. } => bounds(*start, *end),
        }
    }

    pub(super) fn encode(&self, w: &mut Writer) {
        w.var(u64::from(self.field));
        w.var(u64::from(self.source));
        w.str(&self.name);
        match &self.kind {
            GroupKind::Named(groups) => {
                w.u8(0);
                w.var(groups.len() as u64);
                for group in groups {
                    w.str(&group.name);
                    w.var(group.members.len() as u64);
                    for member in &group.members {
                        w.str(member);
                    }
                }
            }
            GroupKind::Discrete(items) => {
                w.u8(4);
                w.var(items.len() as u64);
                for (source, target) in items {
                    put_value(w, source);
                    put_value(w, target);
                }
            }
            GroupKind::NumericBins { start, end, step, items } => {
                w.u8(5);
                w.f64(*start);
                w.f64(*end);
                w.f64(*step);
                w.var(items.len() as u64);
                for item in items {
                    put_value(w, item);
                }
            }
            GroupKind::Intervals {
                start,
                end,
                step,
                dates,
            } => {
                w.u8(if *dates { 2 } else { 1 });
                put_bounds(w, *start, *end);
                w.f64(*step);
            }
            GroupKind::DateParts { start, end, part } => {
                w.u8(3);
                put_bounds(w, *start, *end);
                w.u8(*part as u8);
            }
        }
    }

    pub(super) fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let field = super::get_field(r)?;
        let source = super::get_field(r)?;
        let name = Arc::from(r.str("group field name", 255)?);
        let kind = match r.u8()? {
            0 => {
                let n = r.count("item groups", MAX_GROUP_MEMBERS as u64, 2)?;
                let mut groups = Vec::with_capacity(n.min(1024));
                let mut left = MAX_GROUP_MEMBERS;
                for _ in 0..n {
                    let name = Arc::from(r.str("item group name", 32_767)?);
                    let n = r.count("item group members", left as u64, 1)?;
                    left -= n;
                    let mut members = Vec::with_capacity(n.min(1024));
                    for _ in 0..n {
                        members.push(Arc::from(r.str("group member", 32_767)?));
                    }
                    groups.push(ItemGroup { name, members });
                }
                GroupKind::Named(groups)
            }
            tag @ 1..=2 => {
                let (start, end) = (get_bound(r)?, get_bound(r)?);
                GroupKind::Intervals {
                    start,
                    end,
                    step: r.f64()?,
                    dates: tag == 2,
                }
            }
            3 => {
                let (start, end) = (get_bound(r)?, get_bound(r)?);
                let part = *DatePart::ALL
                    .get(r.u8()? as usize)
                    .ok_or(WireError::Invalid("pivot date group"))?;
                GroupKind::DateParts { start, end, part }
            }
            4 => {
                let len = r.count("pivot discrete group items", MAX_GROUP_MEMBERS as u64, 2)?;
                let items = (0..len).map(|_| Ok((get_value(r)?, get_value(r)?))).collect::<Result<_, WireError>>()?;
                GroupKind::Discrete(items)
            }
            5 => {
                let (start, end, step) = (r.f64()?, r.f64()?, r.f64()?);
                let len = r.count("pivot numeric group items", (MAX_GROUP_MEMBERS + 2) as u64, 1)?;
                let items = (0..len).map(|_| get_value(r)).collect::<Result<_, _>>()?;
                GroupKind::NumericBins { start, end, step, items }
            }
            _ => return Err(WireError::Invalid("pivot grouping type")),
        };
        Ok(Self {
            field,
            source,
            name,
            kind,
        })
    }
}

fn numeric_bins(start: f64, end: f64, step: f64) -> Option<usize> {
    if ![start, end, step].iter().all(|n| n.is_finite()) || start > end || step <= 0.0 {
        return None;
    }
    let count = ((end - start) / step).ceil().max(1.0);
    (count <= MAX_GROUP_MEMBERS as f64).then_some(count as usize)
}

fn put_bounds(w: &mut Writer, start: Option<f64>, end: Option<f64>) {
    for bound in [start, end] {
        w.u8(u8::from(bound.is_some()));
        if let Some(value) = bound {
            w.f64(value);
        }
    }
}

fn get_bound(r: &mut Reader) -> Result<Option<f64>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.f64()?)),
        _ => Err(WireError::Invalid("pivot grouping bound")),
    }
}

/// The lookup indices and automatic bounds for one pass through the source.
pub(super) struct Grouping<'a> {
    source: u32,
    kind: &'a GroupKind,
    parent: Option<Box<Self>>,
    named: HashMap<String, &'a Arc<str>>,
    discrete: HashMap<Key, &'a Value>,
    start: f64,
    end: f64,
    integer_only: bool,
    auto_end: bool,
}

impl<'a> Grouping<'a> {
    pub fn new(wb: &Workbook, p: &'a Pivot, group: &'a GroupField) -> Self {
        let parent = p.groups.iter().find(|field| group.source >= MAX_COLS && field.field == group.source)
            .map(|field| Box::new(Self::new(wb, p, field)));
        let mut this = Self {
            source: group.source,
            kind: &group.kind,
            parent,
            named: HashMap::new(),
            discrete: HashMap::new(),
            start: f64::INFINITY,
            end: f64::NEG_INFINITY,
            integer_only: true,
            auto_end: false,
        };
        let (start, end) = match &group.kind {
            GroupKind::Named(groups) => {
                for group in groups {
                    for member in &group.members {
                        this.named
                            .entry(member.to_lowercase())
                            .or_insert(&group.name);
                    }
                }
                return this;
            }
            GroupKind::Discrete(items) => {
                this.discrete.extend(items.iter().map(|(source, target)| (key(source), target)));
                return this;
            }
            GroupKind::NumericBins { .. } => return this,
            GroupKind::Intervals { start, end, .. } | GroupKind::DateParts { start, end, .. } => {
                (*start, *end)
            }
        };
        this.auto_end = end.is_none();
        let interval = matches!(group.kind, GroupKind::Intervals { .. });
        if (interval || start.is_none() || end.is_none()) && p.source.r0 < p.source.r1 {
            let mut include = |value| {
                if let Value::Num(n) = value {
                    if n.is_finite() {
                        this.start = this.start.min(n);
                        this.end = this.end.max(n);
                        this.integer_only &= n.fract() == 0.0;
                    }
                }
            };
            if let Some(parent) = &this.parent {
                for row in p.source.r0 + 1..=p.source.r1 {
                    include(parent.item(wb, p, row).0);
                }
            } else {
                let col = p.source.c0 + group.source;
                let range = Rect { r0: p.source.r0 + 1, c0: col, c1: col, ..p.source };
                wb.each(&range, &mut |_, _, value| include(value));
            }
        }
        this.start = start.unwrap_or(this.start);
        this.end = end.unwrap_or(this.end);
        if let GroupKind::Intervals { step, .. } = group.kind {
            this.integer_only &= [this.start, this.end, step]
                .into_iter()
                .all(|n| n.fract() == 0.0);
        }
        this
    }

    pub fn item(&self, wb: &Workbook, p: &Pivot, row: u32) -> (Value, Option<f64>) {
        let addr = || CellAddr::new(p.source.sheet, row, p.source.c0 + self.source);
        let (value, parent_order) = self.parent.as_ref()
            .map_or_else(|| (wb.get(addr()), None), |parent| parent.item(wb, p, row));
        if let GroupKind::Named(_) = self.kind {
            let member = match &value {
                Value::Empty => String::new(),
                Value::Str(text) => text.to_lowercase(),
                _ if self.parent.is_none() => wb.formatted(addr()).text.to_lowercase(),
                _ => value.display().to_lowercase(),
            };
            return match self.named.get(&member) {
                Some(name) => (Value::Str((*name).clone()), None),
                None => (value, parent_order),
            };
        }
        if matches!(self.kind, GroupKind::Discrete(_)) {
            return (self.discrete.get(&key(&value)).map_or(value, |target| (*target).clone()), parent_order);
        }
        let Value::Num(n) = value else {
            return (value, None);
        };
        if !n.is_finite() {
            return (value, None);
        }
        if let GroupKind::NumericBins { start, end, step, items } = self.kind {
            let last = items.len() - 1;
            let index = if n < *start { 0 }
                else if n > *end { last }
                else { (1 + ((n - start) / step).floor() as usize).min(last - 1) };
            return (items[index].clone(), Some(index as f64));
        }
        let calendar = wb.date_system();
        let dates = matches!(
            self.kind,
            GroupKind::Intervals { dates: true, .. } | GroupKind::DateParts { .. }
        );
        let bound = |n: f64| {
            if dates {
                date_label(calendar, n)
            } else {
                n.to_string()
            }
        };
        if n < self.start {
            return (
                Value::str(&format!("<{}", bound(self.start))),
                Some(f64::NEG_INFINITY),
            );
        }
        if n > self.end {
            return (
                Value::str(&format!(">{}", bound(self.end))),
                Some(f64::INFINITY),
            );
        }
        match self.kind {
            GroupKind::Intervals { step, .. } => {
                let offset = (n - self.start) / step;
                if !offset.is_finite() {
                    return (value, None);
                }
                let nearest = offset.round();
                let on_edge =
                    (offset - nearest).abs() <= offset.abs().max(1.0) * f64::EPSILON * 4.0;
                let mut bucket = if on_edge { nearest } else { offset.floor() };
                let closes_at_boundary = n == self.end && on_edge && bucket > 0.0;
                if closes_at_boundary {
                    if dates {
                        return (
                            Value::str(&format!(">{}", bound(self.end))),
                            Some(f64::INFINITY),
                        );
                    }
                    bucket -= 1.0;
                }
                let first = self.start + bucket * step;
                let mut last = first + step;
                if self.integer_only && (dates || last != self.end) {
                    last -= 1.0;
                }
                if !self.auto_end {
                    last = last.min(self.end);
                }
                let separator = if dates { " - " } else { "-" };
                (
                    Value::str(&format!("{}{separator}{}", bound(first), bound(last))),
                    Some(bucket),
                )
            }
            GroupKind::DateParts { part, .. } => date_part(calendar, n, *part)
                .map_or((value, None), |(name, rank)| {
                    (Value::str(&name), Some(rank as f64))
                }),
            GroupKind::Named(_) | GroupKind::Discrete(_) | GroupKind::NumericBins { .. } => unreachable!(),
        }
    }
}

fn date_label(calendar: DateSystem, value: f64) -> String {
    calendar.ymd(value).map_or_else(
        || value.to_string(),
        |(y, m, d)| format!("{y:04}-{m:02}-{d:02}"),
    )
}

fn date_part(calendar: DateSystem, value: f64, part: DatePart) -> Option<(String, i64)> {
    let (y, m, d) = calendar.ymd(value)?;
    let seconds = ((value.fract() * 86_400.0 + 0.000001).floor() as i64).min(86_399);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    Some(match part {
        DatePart::Seconds => (format!(":{:02}", seconds % 60), seconds % 60),
        DatePart::Minutes => (format!(":{:02}", seconds / 60 % 60), seconds / 60 % 60),
        DatePart::Hours => (format!("{:02}", seconds / 3600), seconds / 3600),
        DatePart::Days => (format!("{d:02}-{}", months[(m - 1) as usize]), m * 32 + d),
        DatePart::Months => (months[(m - 1) as usize].to_owned(), m),
        DatePart::Quarters => {
            let quarter = (m - 1) / 3 + 1;
            (format!("Q{quarter}"), quarter)
        }
        DatePart::Years => (y.to_string(), y),
    })
}
