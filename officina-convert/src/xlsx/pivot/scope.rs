//! Excel's auto-sort scope selects a measure and optionally an opposite-axis cell.

use std::fmt::Write as _;

use officina_grid::pivot::{AxisSort, Pivot, same_item};
use officina_grid::{Value, Workbook};

const VALUES_FIELD: u32 = u32::MAX - 1;

pub(super) struct Scope<'a> {
    data: usize,
    pub descending: bool,
    members: &'a [(u32, Value)],
}

impl<'a> Scope<'a> {
    pub fn new(wb: &Workbook, pivot: &Pivot, sort: &'a AxisSort) -> Option<Self> {
        let (data, descending, members) = match sort {
            AxisSort::Data {
                caption,
                descending,
            } => (wb.pivot_sort_value(pivot, caption)?.0, *descending, &[][..]),
            AxisSort::Scoped {
                data,
                descending,
                members,
            } => (*data as usize, *descending, members.as_slice()),
            _ => return None,
        };
        Some(Self {
            data,
            descending,
            members,
        })
    }

    pub fn write(&self, out: &mut String, wb: &Workbook, pivot: &Pivot, level: usize, ids: &super::Fields) {
        let count = self.members.len() + 1;
        let _ = write!(
            out,
            r#"<autoSortScope><pivotArea dataOnly="0" outline="0" fieldPosition="{level}"><references count="{count}">"#
        );
        write_reference(out, VALUES_FIELD, self.data);
        let mut members: Vec<_> = self.members.iter().collect();
        members.sort_unstable_by_key(|(field, _)| *field);
        for (field, value) in members {
            let items = super::field_items(wb, pivot, *field);
            let index = items
                .iter()
                .position(|item| same_item(item, value))
                .expect("sort scope members are included in the exported items");
            write_reference(out, ids.raw(*field), index);
        }
        out.push_str("</references></pivotArea></autoSortScope>");
    }
}

fn write_reference(out: &mut String, field: u32, index: usize) {
    let _ = write!(
        out,
        r#"<reference field="{field}" count="1" selected="0"><x v="{index}"/></reference>"#
    );
}

#[cfg(feature = "read")]
pub(super) use read::ScopeIn;

#[cfg(feature = "read")]
mod read {
    use super::{AxisSort, VALUES_FIELD, Value};
    use crate::xlsx::read::{XlsxError, attr, xml};
    use officina_grid::pivot::{MAX_FIELDS, PivotField};
    use quick_xml::events::Event;

    #[derive(Debug)]
    pub(in super::super) struct ScopeIn {
        cache_index: bool,
        references: Vec<(u32, u32)>,
    }

    impl ScopeIn {
        /// Parse just the scope. Positional, relative and multiple selections can
        /// pick different cells than a fixed member and need a separate model.
        pub fn read(data: &[u8]) -> Result<Option<Self>, XlsxError> {
            let mut reader = xml(data);
            let mut buf = Vec::new();
            let mut cache_index = None;
            let mut references = Vec::new();
            let mut reference = None;
            loop {
                buf.clear();
                let event = reader.read_event_into(&mut buf)?;
                let start = matches!(event, Event::Start(_));
                match event {
                    Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                        b"pivotArea" if cache_index.is_none() => {
                            if attr(&e, b"offset")?.is_some()
                                || on(attr(&e, b"labelOnly")?)
                                || attr(&e, b"type")?
                                    .is_some_and(|kind| !matches!(kind.as_str(), "normal" | "data"))
                            {
                                return Ok(None);
                            }
                            cache_index = Some(on(attr(&e, b"cacheIndex")?));
                        }
                        b"references" if cache_index.is_some() => {}
                        b"reference" if start && reference.is_none() && cache_index.is_some() => {
                            let field = attr(&e, b"field")?.and_then(|field| {
                                if field == "-2" {
                                    Some(VALUES_FIELD)
                                } else {
                                    field.parse::<u32>().ok()
                                }
                            });
                            let Some(field) = field else { return Ok(None) };
                            if references.len() > MAX_FIELDS
                                || references.iter().any(|(id, _)| *id == field)
                                || on(attr(&e, b"byPosition")?)
                                || on(attr(&e, b"relative")?)
                            {
                                return Ok(None);
                            }
                            for name in [
                                b"sumSubtotal".as_slice(),
                                b"countASubtotal",
                                b"avgSubtotal",
                                b"maxSubtotal",
                                b"minSubtotal",
                                b"productSubtotal",
                                b"countSubtotal",
                                b"stdDevSubtotal",
                                b"stdDevPSubtotal",
                                b"varSubtotal",
                                b"varPSubtotal",
                            ] {
                                if on(attr(&e, name)?) {
                                    return Ok(None);
                                }
                            }
                            reference = Some((field, None));
                        }
                        b"x" => {
                            let Some((_, index)) = &mut reference else {
                                return Ok(None);
                            };
                            if index.is_some() {
                                return Ok(None);
                            }
                            *index = attr(&e, b"v")?.map_or(Some(0), |raw| raw.parse::<u32>().ok());
                            if index.is_none() {
                                return Ok(None);
                            }
                        }
                        _ => return Ok(None),
                    },
                    Event::End(e) if e.local_name().as_ref() == b"reference" => {
                        let Some((field, Some(index))) = reference.take() else {
                            return Ok(None);
                        };
                        references.push((field, index));
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
            Ok(cache_index.map(|cache_index| Self {
                cache_index,
                references,
            }))
        }

        pub fn resolve(
            &self,
            order: Option<&str>,
            opposite: &[PivotField],
            data_fields: &[Option<u32>],
            member: impl Fn(u32, u32, bool) -> Option<Value>,
        ) -> Option<AxisSort> {
            let descending = match order? {
                "ascending" => false,
                "descending" => true,
                _ => return None,
            };
            let data = match self
                .references
                .iter()
                .find(|(field, _)| *field == VALUES_FIELD)
            {
                Some((_, index)) => (*data_fields.get(*index as usize)?)?,
                None if data_fields.len() == 1 => data_fields[0]?,
                None => return None,
            };
            let count = self
                .references
                .iter()
                .filter(|(field, _)| *field != VALUES_FIELD)
                .count();
            if count > opposite.len() {
                return None;
            }
            let members = opposite[..count]
                .iter()
                .map(|f| {
                    let (_, index) = self
                        .references
                        .iter()
                        .find(|(field, _)| *field == f.field)?;
                    Some((f.field, member(f.field, *index, self.cache_index)?))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(AxisSort::Scoped {
                data,
                descending,
                members,
            })
        }
    }

    fn on(value: Option<String>) -> bool {
        value.is_some_and(|v| matches!(v.as_str(), "1" | "true"))
    }
}
