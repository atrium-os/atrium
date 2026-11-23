//! A pivot cache can retain its worksheet source even if its cached items are gone.

use crate::xlsx::read::{XlsxError, attr, internal_rels, parse_ref, xml};
use crate::xlsx::write::{XLSX_COLS, XLSX_ROWS};
use crate::zip::Archive;
use officina_grid::addr::{MAX_ROWS, Rect};
use officina_grid::eval::dates::DateSystem;
use officina_grid::formula::ast::{Coord, Expr};
use officina_grid::pivot::MAX_ITEMS;
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
    fields: Vec<Option<Vec<Value>>>,
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
                        if self.fields.len() >= XLSX_COLS as usize {
                            self.unsupported = Some("it has too many cache fields");
                            break;
                        }
                        if attr(&e, b"formula")?.is_some()
                            || attr(&e, b"databaseField")?
                                .is_some_and(|v| matches!(v.as_str(), "0" | "false"))
                        {
                            self.unsupported = Some("it uses calculated Excel cache fields");
                        }
                        let parsed = if start {
                            let span = reader.read_to_end(e.name())?;
                            cache_items(&data[span.start as usize..span.end as usize], dates)?
                        } else {
                            FieldItems {
                                items: Some(Vec::new()),
                                grouped: false,
                            }
                        };
                        if parsed.grouped {
                            self.unsupported = Some("it uses grouped Excel cache fields");
                        }
                        self.rebuilt |= parsed.items.is_none();
                        self.fields.push(parsed.items);
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
        self.fields.get(field as usize)?.as_deref()
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
        self.rebuilt || self.fields.len() != (source.c1 - source.c0 + 1) as usize
    }
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

struct FieldItems {
    items: Option<Vec<Value>>,
    grouped: bool,
}

fn cache_items(data: &[u8], dates: DateSystem) -> Result<FieldItems, XlsxError> {
    let mut reader = xml(data);
    let mut out = FieldItems {
        items: Some(Vec::new()),
        grouped: false,
    };
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
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"fieldGroup" => {
                out.grouped = true
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
