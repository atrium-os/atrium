//! Slicers and timelines (docs/spec/officina.md §16.164): buttons on the sheet
//! that filter a pivot by one field's items, or by a date range. A slicer's
//! selection is the field's hidden items, where the field is in the layout or
//! in the pivot's filters; a timeline's is a filter's date range. Neither shows
//! in the pivot's cells, as Excel's.

use super::{DatePart, Pivot, PivotField, MAX_FIELDS, MAX_ITEMS};
use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::charts::Anchor;
use crate::value::Value;
use officina_core::wire::{Reader, WireError, Writer};
use std::sync::Arc;

/// Slicers and timelines one pivot may have.
pub const MAX_SLICERS: usize = 32;

/// A filter on a field that need not be in the layout: hidden items, and a
/// range of source values (dates as serials) from `.0` up to but not
/// including `.1`.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotFilter {
    pub field: u32,
    pub hidden: Vec<Value>,
    pub between: Option<(f64, f64)>,
}

/// A slicer (`timeline` `None`) or a timeline (by years, quarters, months or
/// days) for one field of its pivot.
#[derive(Debug, Clone, PartialEq)]
pub struct Slicer {
    pub name: Arc<str>,
    pub field: u32,
    pub anchor: Anchor,
    pub timeline: Option<DatePart>,
}

impl Slicer {
    pub fn timeline_part_ok(part: DatePart) -> bool {
        matches!(part, DatePart::Years | DatePart::Quarters | DatePart::Months | DatePart::Days)
    }
}

impl Pivot {
    /// The items of `field` not shown: from the layout field, else its filter.
    pub fn hidden_of(&self, field: u32) -> &[Value] {
        if let Some(f) = self.rows.iter().chain(&self.cols).chain(&self.pages).find(|f| f.field == field) {
            return &f.hidden;
        }
        self.filters.iter().find(|f| f.field == field).map_or(&[], |f| &f.hidden)
    }

    /// Sets the items of `field` not shown, where the field is (a filter of
    /// its own when it isn't in the layout; none left when nothing is hidden
    /// and it has no range).
    pub fn set_hidden(&mut self, field: u32, hidden: Vec<Value>) {
        if let Some(f) = self.rows.iter_mut().chain(&mut self.cols).chain(&mut self.pages).find(|f| f.field == field) {
            f.hidden = hidden;
            return;
        }
        match self.filters.iter().position(|f| f.field == field) {
            Some(i) => self.filters[i].hidden = hidden,
            None => self.filters.push(PivotFilter { field, hidden, between: None }),
        }
        self.filters.retain(|f| !f.hidden.is_empty() || f.between.is_some());
    }

    /// The date range a timeline on `field` keeps, if any.
    pub fn range_of(&self, field: u32) -> Option<(f64, f64)> {
        self.filters.iter().find(|f| f.field == field).and_then(|f| f.between)
    }

    pub fn set_range(&mut self, field: u32, between: Option<(f64, f64)>) {
        match self.filters.iter().position(|f| f.field == field) {
            Some(i) => self.filters[i].between = between,
            None => self.filters.push(PivotFilter { field, hidden: Vec::new(), between }),
        }
        self.filters.retain(|f| !f.hidden.is_empty() || f.between.is_some());
    }

    /// The fields to filter rows by that aren't in the layout.
    pub(super) fn filter_only(&self) -> impl Iterator<Item = &PivotFilter> {
        self.filters.iter().filter(|f| !f.hidden.is_empty() && !self.rows.iter().chain(&self.cols).chain(&self.pages).any(|x| x.field == f.field))
    }

    pub(super) fn check_filters(&self, width: u32, sheets: usize) -> Result<(), &'static str> {
        if self.filters.len() > MAX_FIELDS || self.slicers.len() > MAX_SLICERS {
            return Err("too many pivot filters or slicers");
        }
        let derived = |f: u32| self.groups.iter().any(|g| g.field == f);
        for (i, f) in self.filters.iter().enumerate() {
            if self.filters[..i].iter().any(|g| g.field == f.field) || f.hidden.len() > MAX_ITEMS {
                return Err("an invalid pivot filter");
            }
            if f.field >= width && !derived(f.field) {
                return Err("a pivot filter on a field outside the source");
            }
            if let Some((a, b)) = f.between {
                if !(a.is_finite() && b.is_finite() && a < b) || f.field >= width {
                    return Err("an invalid pivot date range");
                }
            }
        }
        for (i, s) in self.slicers.iter().enumerate() {
            if s.name.is_empty() || s.name.len() > 255 || self.slicers[..i].iter().any(|t| t.name.eq_ignore_ascii_case(&s.name)) {
                return Err("an invalid slicer name");
            }
            if (s.field >= width && !derived(s.field)) || s.timeline.is_some_and(|p| !Slicer::timeline_part_ok(p) || s.field >= width) {
                return Err("an invalid slicer field");
            }
            let a = &s.anchor;
            if a.sheet as usize >= sheets || a.from.0 > a.to.0 || a.from.1 > a.to.1 || a.to.0 >= MAX_ROWS || a.to.1 >= MAX_COLS {
                return Err("a slicer outside the sheet");
            }
        }
        Ok(())
    }

    pub(super) fn has_extras(&self) -> bool {
        !self.filters.is_empty() || !self.slicers.is_empty()
    }

    pub(super) fn encode_extras(&self, w: &mut Writer, put_value: impl Fn(&mut Writer, &Value)) {
        w.var(self.filters.len() as u64);
        for f in &self.filters {
            w.var(u64::from(f.field));
            w.var(f.hidden.len() as u64);
            for v in &f.hidden {
                put_value(w, v);
            }
            match f.between {
                Some((a, b)) => {
                    w.u8(1);
                    w.f64(a);
                    w.f64(b);
                }
                None => w.u8(0),
            }
        }
        w.var(self.slicers.len() as u64);
        for s in &self.slicers {
            w.str(&s.name);
            w.var(u64::from(s.field));
            let a = &s.anchor;
            w.u16(a.sheet);
            for (r, c, x, y) in [a.from, a.to] {
                w.var(u64::from(r));
                w.var(u64::from(c));
                w.f64(f64::from(x));
                w.f64(f64::from(y));
            }
            w.u8(s.timeline.map_or(0, |p| 1 + DatePart::ALL.iter().position(|q| *q == p).unwrap_or(0) as u8));
        }
    }

    pub(super) fn decode_extras(
        r: &mut Reader,
        get_field: impl Fn(&mut Reader) -> Result<u32, WireError>,
        get_value: impl Fn(&mut Reader) -> Result<Value, WireError>,
    ) -> Result<(Vec<PivotFilter>, Vec<Slicer>), WireError> {
        let n = r.count("pivot filters", MAX_FIELDS as u64, 3)?;
        let mut filters = Vec::with_capacity(n);
        for _ in 0..n {
            let field = get_field(r)?;
            let h = r.count("filter items", MAX_ITEMS as u64, 1)?;
            let mut hidden = Vec::with_capacity(h.min(1024));
            for _ in 0..h {
                hidden.push(get_value(r)?);
            }
            let between = match r.u8()? {
                0 => None,
                1 => {
                    let (a, b) = (r.f64()?, r.f64()?);
                    if !(a.is_finite() && b.is_finite() && a < b) {
                        return Err(WireError::Invalid("pivot date range"));
                    }
                    Some((a, b))
                }
                _ => return Err(WireError::Invalid("pivot filter")),
            };
            filters.push(PivotFilter { field, hidden, between });
        }
        let n = r.count("slicers", MAX_SLICERS as u64, 8)?;
        let mut slicers = Vec::with_capacity(n);
        for _ in 0..n {
            let name = Arc::from(r.str("slicer name", 255)?);
            let field = get_field(r)?;
            let sheet = r.u16()?;
            let mut corner = || -> Result<(u32, u32, f32, f32), WireError> {
                let (row, col) = (r.var()?, r.var()?);
                let (x, y) = (r.f64()?, r.f64()?);
                if row >= u64::from(MAX_ROWS) || col >= u64::from(MAX_COLS) || !((0.0..=1e6).contains(&x) && (0.0..=1e6).contains(&y)) {
                    return Err(WireError::Invalid("slicer place"));
                }
                Ok((row as u32, col as u32, x as f32, y as f32))
            };
            let (from, to) = (corner()?, corner()?);
            let timeline = match r.u8()? {
                0 => None,
                k => {
                    let p = *DatePart::ALL.get(k as usize - 1).ok_or(WireError::Invalid("timeline level"))?;
                    if !Slicer::timeline_part_ok(p) {
                        return Err(WireError::Invalid("timeline level"));
                    }
                    Some(p)
                }
            };
            slicers.push(Slicer { name, field, anchor: Anchor { sheet, from, to }, timeline });
        }
        Ok((filters, slicers))
    }
}

/// A new slicer or timeline's name: the field's, then "Field 1", "Field 2"…
pub fn slicer_name(p: &Pivot, field_name: &str) -> String {
    let base = if field_name.is_empty() { "Slicer" } else { field_name };
    std::iter::once(base.to_string())
        .chain((1..).map(|i| format!("{base} {i}")))
        .find(|n| !p.slicers.iter().any(|s| s.name.eq_ignore_ascii_case(n)))
        .unwrap_or_default()
}

/// Whether `f` is a layout field (for callers that build filters).
pub fn in_layout(p: &Pivot, f: u32) -> bool {
    let has = |v: &[PivotField]| v.iter().any(|x| x.field == f);
    has(&p.rows) || has(&p.cols) || has(&p.pages)
}
