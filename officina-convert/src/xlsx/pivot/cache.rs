//! A pivot cache can retain its worksheet source even if its cached items are gone.

use crate::xlsx::read::{XlsxError, attr, internal_rels, parse_ref, xml};
use crate::xlsx::write::{XLSX_COLS, XLSX_ROWS};
use crate::zip::Archive;
use officina_grid::addr::{MAX_ROWS, Rect};
use officina_grid::eval::dates::DateSystem;
use officina_grid::formula::ast::{Coord, Expr};
use officina_grid::pivot::{CalculatedField, GroupField, GroupKind, MAX_GROUP_MEMBERS, MAX_GROUPS, MAX_ITEMS, Pivot};
use std::sync::Arc;
use officina_grid::{ErrorKind, Value, Workbook};
use quick_xml::events::{BytesStart, Event};

#[derive(Default)]
struct Source {
    sheet: Option<String>,
    reference: Option<String>,
    name: Option<String>,
    relationship: Option<String>,
}

#[derive(Default)]
pub(super) struct Cache {
    source: Option<Source>,
    fields: Vec<FieldItems>,
    pub rebuilt: bool,
    unsupported: Option<&'static str>,
}

impl Cache {
    pub fn read(data: &[u8], dates: DateSystem) -> Self {
        let mut cache = Self::default();
        if cache.parse(data, dates).is_err() {
            cache.rebuilt = true;
        }
        cache
    }

    fn parse(&mut self, data: &[u8], dates: DateSystem) -> Result<(), XlsxError> {
        let mut reader = xml(data);
        let mut root = false;
        let mut fields = false;
        let mut declared = None;
        loop {
            let event = reader.read_event()?;
            let start = matches!(event, Event::Start(_));
            match event {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"pivotCacheDefinition" if start && !root => {
                        root = true;
                        self.rebuilt |= on(attr(&e, b"invalid")?);
                    }
                    b"cacheSource" if start && root => {
                        let span = reader.read_to_end(e.name())?;
                        if attr(&e, b"type")?.is_some_and(|kind| kind != "worksheet") {
                            self.unsupported = Some("its source is external or is not a worksheet");
                        } else if self.source.is_none() {
                            self.source =
                                worksheet_source(&data[span.start as usize..span.end as usize])?;
                        } else {
                            self.unsupported = Some("it specifies more than one source");
                        }
                    }
                    b"cacheFields" if root => {
                        fields = start;
                        declared =
                            attr(&e, b"count")?.and_then(|count| count.parse::<usize>().ok());
                    }
                    b"cacheField" if fields => {
                        if self.fields.len() >= XLSX_COLS as usize + MAX_GROUPS {
                            self.unsupported = Some("it has too many cache fields");
                            break;
                        }
                        let name = attr(&e, b"name")?.unwrap_or_default();
                        let formula = attr(&e, b"formula")?;
                        let derived = off(attr(&e, b"databaseField")?) || formula.is_some();
                        let mut parsed = if start {
                            let span = reader.read_to_end(e.name())?;
                            cache_items(&data[span.start as usize..span.end as usize], dates)?
                        } else {
                            FieldItems { items: Some(Vec::new()), ..FieldItems::default() }
                        };
                        parsed.name = name;
                        parsed.formula = formula;
                        parsed.derived = derived;
                        self.rebuilt |= parsed.items.is_none();
                        self.fields.push(parsed);
                    }
                    b"calculatedItems" if start && root => {
                        self.unsupported = Some("it uses calculated Excel cache items");
                    }
                    _ => {}
                },
                Event::End(e) if e.local_name().as_ref() == b"cacheFields" => fields = false,
                Event::End(e) if root && e.local_name().as_ref() == b"pivotCacheDefinition" => {
                    self.rebuilt |= declared.is_some_and(|count| count != self.fields.len());
                    return Ok(());
                }
                Event::Eof => break,
                _ => {}
            }
        }
        self.rebuilt = true;
        Ok(())
    }

    pub fn items(&self, field: u32) -> Option<&[Value]> {
        let field = self.fields.get(field as usize)?;
        match &field.group {
            Some(group) if group.mode.is_some() => group.items.as_deref(),
            _ => field.items.as_deref(),
        }
    }

    /// Resolve cache metadata into the grid's virtual fields after the source range is known.
    pub fn definitions(&self, pivot: &mut Pivot) -> Result<(), &'static str> {
        let width = pivot.source.c1 - pivot.source.c0 + 1;
        let invalid = "it uses an unsupported or incomplete grouped Excel cache";
        for (raw, field) in self.fields.iter().enumerate() {
            let raw = raw as u32;
            let id = super::model_field(width, raw);
            if let Some(formula) = &field.formula {
                if raw < width || field.group.is_some() {
                    return Err("its calculated field overlaps the worksheet source");
                }
                pivot.calculations.push(CalculatedField {
                    field: id, name: Arc::from(field.name.as_str()), formula: Arc::from(formula.as_str()),
                });
                continue;
            }
            if raw >= width && !field.derived && field.group.is_none() {
                // An old cache may outlive a source that was narrowed. Axes or
                // measures that still address the extra columns fail validation.
                continue;
            }
            if field.derived != (raw >= width) {
                return Err("its cache fields do not match the worksheet source");
            }
            let Some(group) = &field.group else {
                if field.derived { return Err(invalid); }
                continue;
            };
            let Some(mode) = &group.mode else {
                if group.parent.is_some() && !field.derived { continue; }
                return Err(invalid);
            };
            let base = group.base.filter(|&base| base < width).ok_or(invalid)?;
            let items = group.items.as_ref().filter(|items| (1..=MAX_GROUP_MEMBERS + 2).contains(&items.len())).ok_or(invalid)?;
            let unique: std::collections::HashSet<_> = items.iter().map(officina_grid::pivot::item_key).collect();
            if unique.len() != items.len() { return Err(invalid); }
            let (parent, kind) = match mode {
                GroupMode::Discrete(indices) => {
                    if raw < width { return Err(invalid); }
                    let mut children = self.fields.iter().enumerate().filter(|(_, child)| {
                        child.group.as_ref().is_some_and(|child| child.parent == Some(raw))
                    });
                    let (child, input) = match children.next() {
                        Some((child, _)) if children.next().is_none() =>
                            (child as u32, self.items(child as u32).ok_or(invalid)?),
                        None => (base, self.fields.get(base as usize).and_then(|f| f.items.as_deref()).ok_or(invalid)?),
                        _ => return Err(invalid),
                    };
                    if input.len() != indices.len() { return Err(invalid); }
                    let mapping = input.iter().zip(indices).map(|(from, &to)| {
                        items.get(to).map(|to| (from.clone(), to.clone())).ok_or(invalid)
                    }).collect::<Result<Vec<_>, _>>()?;
                    (child, GroupKind::Discrete(mapping.into_iter().filter(|(from, to)| from != to).collect()))
                }
                GroupMode::Numeric { start, end, step } => (base, GroupKind::NumericBins {
                    start: *start, end: *end, step: *step, items: items.clone(),
                }),
                GroupMode::Unsupported => return Err(invalid),
            };
            pivot.groups.push(GroupField { field: id, source: super::model_field(width, parent),
                name: Arc::from(field.name.as_str()), kind });
        }
        pivot.check_groups()
    }

    pub fn resolve(
        &self,
        wb: &Workbook,
        archive: &Archive,
        part: &str,
        sheets: &[String],
        pivot_sheet: u16,
    ) -> Result<Rect, &'static str> {
        if let Some(reason) = self.unsupported {
            return Err(reason);
        }
        let source = self
            .source
            .as_ref()
            .ok_or("the cache has no local worksheet source")?;
        let from_name = source
            .sheet
            .as_deref()
            .map(|name| {
                wb.sheet_id(name)
                    .ok_or("its source worksheet is unavailable")
            })
            .transpose()?;
        let from_link = source
            .relationship
            .as_deref()
            .map(|id| {
                let links = internal_rels(archive, part)
                    .map_err(|_| "its worksheet source relationship is unreadable")?;
                let (_, path) = links
                    .get(id)
                    .filter(|(kind, _)| kind.ends_with("/worksheet"))
                    .ok_or("its worksheet source relationship is external or unavailable")?;
                sheets
                    .iter()
                    .position(|sheet| sheet == path)
                    .map(|sheet| sheet as u16)
                    .ok_or("its worksheet source relationship points outside this workbook")
            })
            .transpose()?;
        if from_name.zip(from_link).is_some_and(|(a, b)| a != b) {
            return Err("its worksheet source identifiers disagree");
        }
        let explicit_sheet = from_name.or(from_link);
        let sheet = explicit_sheet.unwrap_or(pivot_sheet);
        let source_rect = if let Some(reference) = &source.reference {
            let (r0, c0, r1, c1) =
                parse_ref(reference).ok_or("its worksheet source range is invalid")?;
            Rect {
                sheet,
                r0,
                c0,
                r1,
                c1,
            }
        } else if let Some(name) = &source.name {
            let rect =
                named_source(wb, sheet, name).ok_or("its source name has no fixed local range")?;
            if explicit_sheet.is_some_and(|sheet| sheet != rect.sheet) {
                return Err("its source name and worksheet disagree");
            }
            rect
        } else {
            return Err("its source has no worksheet range or name");
        };
        let mut rect = source_rect;
        if rect.r1 == XLSX_ROWS - 1 || rect.r1 == MAX_ROWS - 1 {
            rect.r1 = rect
                .r1
                .min(wb.extent(rect.sheet).0.saturating_sub(1).max(rect.r0));
        }
        if rect.r0 > rect.r1 || rect.c0 > rect.c1 || rect.r1 >= XLSX_ROWS || rect.c1 >= XLSX_COLS {
            return Err("its worksheet source range is invalid");
        }
        Ok(rect)
    }

    pub fn needs_rebuild(&self, source: Rect) -> bool {
        self.rebuilt || self.fields.iter().filter(|f| !f.derived).count() != (source.c1 - source.c0 + 1) as usize
    }
}

fn off(value: Option<String>) -> bool {
    value.is_some_and(|value| matches!(value.as_str(), "false" | "0"))
}

fn on(value: Option<String>) -> bool {
    value.is_some_and(|value| matches!(value.as_str(), "true" | "1"))
}

fn worksheet_source(data: &[u8]) -> Result<Option<Source>, XlsxError> {
    let mut reader = xml(data);
    let mut source = None;
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"worksheetSource" => {
                if source.is_some() {
                    return Ok(None);
                }
                source = Some(Source {
                    sheet: attr(&e, b"sheet")?,
                    reference: attr(&e, b"ref")?,
                    name: attr(&e, b"name")?,
                    relationship: attr(&e, b"id")?,
                });
            }
            Event::Eof => return Ok(source),
            _ => {}
        }
    }
}

fn named_source(wb: &Workbook, sheet: u16, name: &str) -> Option<Rect> {
    if let Some(table) = wb.table(name) {
        return table.header.then(|| Rect {
            r1: table.rect.r1.saturating_sub(u32::from(table.totals)),
            ..table.rect
        });
    }
    let def = wb.names().get(sheet, name)?;
    let Expr::Range(range) = def.expr.as_ref() else {
        return None;
    };
    let (Coord::Abs(r0), Coord::Abs(c0), Coord::Abs(r1), Coord::Abs(c1)) =
        (range.r0, range.c0, range.r1, range.c1)
    else {
        return None;
    };
    // A worksheet-scoped name can supply the sheet when its range omits it.
    let sheet = range.sheet.or(def.scope)?;
    Some(Rect {
        sheet,
        r0,
        c0,
        r1,
        c1,
    })
}

#[derive(Default)]
struct FieldItems {
    name: String,
    formula: Option<String>,
    derived: bool,
    items: Option<Vec<Value>>,
    group: Option<GroupIn>,
}

struct GroupIn {
    base: Option<u32>,
    parent: Option<u32>,
    mode: Option<GroupMode>,
    items: Option<Vec<Value>>,
}

enum GroupMode {
    Discrete(Vec<usize>),
    Numeric { start: f64, end: f64, step: f64 },
    Unsupported,
}

fn cache_group(tag: &BytesStart<'_>, data: &[u8], dates: DateSystem) -> Result<GroupIn, XlsxError> {
    let mut group = GroupIn {
        base: attr(tag, b"base")?.and_then(|v| v.parse().ok()),
        parent: attr(tag, b"par")?.and_then(|v| v.parse().ok()),
        mode: None,
        items: None,
    };
    let mut reader = xml(data);
    loop {
        let event = reader.read_event()?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => match tag.local_name().as_ref() {
                b"groupItems" if start && group.items.is_none() => {
                    let count = attr(&tag, b"count")?.and_then(|v| v.parse().ok());
                    let span = reader.read_to_end(tag.name())?;
                    group.items = shared_items(&data[span.start as usize..span.end as usize], dates, count)?;
                }
                b"discretePr" if start && group.mode.is_none() => {
                    let count = attr(&tag, b"count")?.and_then(|v| v.parse::<usize>().ok());
                    let span = reader.read_to_end(tag.name())?;
                    let mut indices = Vec::new();
                    let mut items = xml(&data[span.start as usize..span.end as usize]);
                    group.mode = Some(loop {
                        match items.read_event()? {
                            Event::Empty(x) if x.local_name().as_ref() == b"x" && indices.len() < MAX_GROUP_MEMBERS => {
                                let Some(index) = attr(&x, b"v")?.and_then(|v| v.parse().ok()) else { break GroupMode::Unsupported };
                                indices.push(index);
                            }
                            Event::Eof if count.is_none_or(|n| n == indices.len()) => break GroupMode::Discrete(indices),
                            _ => break GroupMode::Unsupported,
                        }
                    });
                }
                b"rangePr" if group.mode.is_none() => {
                    let numeric = attr(&tag, b"groupBy")?.is_none_or(|kind| kind == "range")
                        && off(attr(&tag, b"autoStart")?) && off(attr(&tag, b"autoEnd")?)
                        && group.parent.is_none() && attr(&tag, b"startDate")?.is_none();
                    let start_num = attr(&tag, b"startNum")?.and_then(|v| v.parse().ok());
                    let end_num = attr(&tag, b"endNum")?.and_then(|v| v.parse().ok());
                    let step = attr(&tag, b"groupInterval")?.and_then(|v| v.parse().ok()).unwrap_or(1.0);
                    group.mode = Some(match (numeric, start_num, end_num) {
                        (true, Some(start), Some(end)) => GroupMode::Numeric { start, end, step },
                        _ => GroupMode::Unsupported,
                    });
                    if start { reader.read_to_end(tag.name())?; }
                }
                _ => {
                    group.mode = Some(GroupMode::Unsupported);
                    if start { reader.read_to_end(tag.name())?; }
                }
            },
            Event::Eof => return Ok(group),
            _ => {}
        }
    }
}

fn cache_items(data: &[u8], dates: DateSystem) -> Result<FieldItems, XlsxError> {
    let mut reader = xml(data);
    let mut out = FieldItems { items: Some(Vec::new()), ..FieldItems::default() };
    let mut seen = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == b"sharedItems" => {
                if seen {
                    out.items = None;
                    return Ok(out);
                }
                let span = reader.read_to_end(e.name())?;
                let count = attr(&e, b"count")?.and_then(|count| count.parse::<usize>().ok());
                out.items =
                    shared_items(&data[span.start as usize..span.end as usize], dates, count)?;
                seen = true;
            }
            Event::Empty(e) if e.local_name().as_ref() == b"sharedItems" => {
                if seen || attr(&e, b"count")?.is_some_and(|count| count != "0") {
                    out.items = None;
                }
                seen = true;
            }
            Event::Start(e) if e.local_name().as_ref() == b"fieldGroup" => {
                let span = reader.read_to_end(e.name())?;
                out.group = Some(cache_group(&e, &data[span.start as usize..span.end as usize], dates)?);
            }
            Event::Empty(e) if e.local_name().as_ref() == b"fieldGroup" => {
                out.group = Some(cache_group(&e, &[], dates)?);
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
    }
}

fn shared_items(
    data: &[u8],
    dates: DateSystem,
    declared: Option<usize>,
) -> Result<Option<Vec<Value>>, XlsxError> {
    let mut reader = xml(data);
    let mut items = Vec::new();
    loop {
        let event = reader.read_event()?;
        let start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                if items.len() == MAX_ITEMS {
                    return Ok(None);
                }
                let Some(item) = item(&e, dates)? else {
                    return Ok(None);
                };
                if start {
                    reader.read_to_end(e.name())?;
                }
                items.push(item);
            }
            Event::Eof => {
                return Ok(declared
                    .is_none_or(|count| count == items.len())
                    .then_some(items));
            }
            _ => {}
        }
    }
}

fn item(e: &BytesStart<'_>, dates: DateSystem) -> Result<Option<Value>, XlsxError> {
    let v = attr(e, b"v")?;
    Ok(match e.local_name().as_ref() {
        b"s" => v.map(|v| Value::str(&v)),
        b"n" => v
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map(Value::Num),
        b"b" => match v.as_deref() {
            Some("1" | "true") => Some(Value::Bool(true)),
            Some("0" | "false") => Some(Value::Bool(false)),
            _ => None,
        },
        b"e" => v.and_then(|v| ErrorKind::from_text(&v)).map(Value::Err),
        b"m" => Some(Value::Empty),
        b"d" => v.map(|v| {
            dates
                .iso(&v)
                .map_or_else(|| Value::str(&v), |(date, _)| Value::Num(date))
        }),
        _ => None,
    })
}
