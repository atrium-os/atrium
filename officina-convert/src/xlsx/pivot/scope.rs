//! Excel's auto-sort scope selects a measure and opposite-axis members.

use std::fmt::Write as _;

use officina_grid::pivot::{same_item, AxisSort, Pivot, ScopeItems};
use officina_grid::Workbook;

const VALUES_FIELD: u32 = u32::MAX - 1;

pub(super) struct Scope {
    data: usize,
    pub descending: bool,
    fields: Vec<(u32, ScopeItems)>,
}

impl Scope {
    pub fn new(wb: &Workbook, pivot: &Pivot, sort: &AxisSort) -> Option<Self> {
        let (data, descending, fields) = match sort {
            AxisSort::Data {
                caption,
                descending,
            } => (
                wb.pivot_sort_value(pivot, caption)?.0,
                *descending,
                Vec::new(),
            ),
            AxisSort::Scoped {
                data,
                descending,
                members,
            } => (
                *data as usize,
                *descending,
                members
                    .iter()
                    .map(|(field, value)| (*field, ScopeItems::Members(vec![value.clone()])))
                    .collect(),
            ),
            AxisSort::Selection {
                data,
                descending,
                fields,
            } => (*data as usize, *descending, fields.clone()),
            _ => return None,
        };
        Some(Self {
            data,
            descending,
            fields,
        })
    }

    pub fn write(
        &self,
        out: &mut String,
        wb: &Workbook,
        pivot: &Pivot,
        level: usize,
        ids: &super::Fields,
    ) {
        let count = self.fields.len() + 1;
        let _ = write!(
            out,
            r#"<autoSortScope><pivotArea dataOnly="0" outline="0" fieldPosition="{level}"><references count="{count}">"#
        );
        write_reference(out, VALUES_FIELD, false, &[self.data]);
        let mut fields: Vec<_> = self.fields.iter().collect();
        fields.sort_unstable_by_key(|(field, _)| *field);
        for (field, selector) in fields {
            let (positions, indices): (bool, Vec<usize>) = match selector {
                ScopeItems::Positions(items) => {
                    (true, items.iter().map(|&item| item as usize).collect())
                }
                ScopeItems::Members(values) => {
                    let items = super::field_items(wb, pivot, *field);
                    let indices = values
                        .iter()
                        .filter_map(|value| items.iter().position(|item| same_item(item, value)))
                        .collect();
                    (false, indices)
                }
            };
            write_reference(out, ids.raw(*field), positions, &indices);
        }
        out.push_str("</references></pivotArea></autoSortScope>");
    }
}

fn write_reference(out: &mut String, field: u32, positions: bool, indices: &[usize]) {
    let position_flag = if positions { r#" byPosition="1""# } else { "" };
    let _ = write!(
        out,
        r#"<reference field="{field}"{position_flag} count="{}" selected="0">"#,
        indices.len()
    );
    for index in indices {
        let _ = write!(out, r#"<x v="{index}"/>"#);
    }
    out.push_str("</reference>");
}

#[cfg(feature = "read")]
pub(super) use read::ScopeIn;

#[cfg(feature = "read")]
mod read {
    use super::{AxisSort, ScopeItems, VALUES_FIELD};
    use crate::xlsx::read::{attr, xml, XlsxError};
    use officina_grid::pivot::{PivotField, MAX_FIELDS, MAX_ITEMS, MAX_SCOPE_ITEMS};
    use officina_grid::Value;
    use quick_xml::events::Event;

    #[derive(Debug)]
    struct Reference {
        field: u32,
        positions: bool,
        items: Vec<u32>,
    }

    #[derive(Debug)]
    pub(in super::super) struct ScopeIn {
        cache_index: bool,
        references: Vec<Reference>,
    }

    impl ScopeIn {
        pub fn read(data: &[u8]) -> Result<Option<Self>, XlsxError> {
            let mut reader = xml(data);
            let mut buf = Vec::new();
            let mut cache_index = None;
            let mut references: Vec<Reference> = Vec::new();
            let mut reference: Option<(Reference, Option<usize>)> = None;
            let mut remaining = MAX_SCOPE_ITEMS + 1;

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
                            let positions = on(attr(&e, b"byPosition")?);
                            if references.len() > MAX_FIELDS
                                || references.iter().any(|parsed| parsed.field == field)
                                || on(attr(&e, b"relative")?)
                                || positions && (cache_index == Some(true) || field == VALUES_FIELD)
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
                            let count = attr(&e, b"count")?.and_then(|count| count.parse().ok());
                            reference = Some((
                                Reference {
                                    field,
                                    positions,
                                    items: Vec::new(),
                                },
                                count,
                            ));
                        }
                        b"x" => {
                            let Some((reference, _)) = &mut reference else {
                                return Ok(None);
                            };
                            let index =
                                attr(&e, b"v")?.map_or(Some(0), |raw| raw.parse::<u32>().ok());
                            let Some(index) = index
                                .filter(|&index| (index as usize) < MAX_ITEMS && remaining > 0)
                            else {
                                return Ok(None);
                            };
                            reference.items.push(index);
                            remaining -= 1;
                        }
                        _ => return Ok(None),
                    },
                    Event::End(e) if e.local_name().as_ref() == b"reference" => {
                        let Some((reference, count)) = reference.take() else {
                            return Ok(None);
                        };
                        if reference.items.is_empty()
                            || count.is_some_and(|count| count != reference.items.len())
                        {
                            return Ok(None);
                        }
                        references.push(reference);
                    }
                    Event::Eof if reference.is_none() => break,
                    Event::Eof => return Ok(None),
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
                .find(|reference| reference.field == VALUES_FIELD)
            {
                Some(reference) if reference.items.len() == 1 => {
                    (*data_fields.get(reference.items[0] as usize)?)?
                }
                None if data_fields.len() == 1 => data_fields[0]?,
                _ => return None,
            };
            let selectors = self
                .references
                .iter()
                .filter(|reference| reference.field != VALUES_FIELD);
            if selectors
                .clone()
                .map(|reference| reference.items.len())
                .sum::<usize>()
                > MAX_SCOPE_ITEMS
            {
                return None;
            }
            let count = selectors.count();
            let fields = opposite
                .get(..count)?
                .iter()
                .map(|field| {
                    let reference = self
                        .references
                        .iter()
                        .find(|reference| reference.field == field.field)?;
                    let selector = if reference.positions {
                        ScopeItems::Positions(reference.items.clone())
                    } else {
                        ScopeItems::Members(
                            reference
                                .items
                                .iter()
                                .map(|&item| member(field.field, item, self.cache_index))
                                .collect::<Option<_>>()?,
                        )
                    };
                    Some((field.field, selector))
                })
                .collect::<Option<Vec<_>>>()?;
            if fields
                .iter()
                .all(|(_, items)| matches!(items, ScopeItems::Members(items) if items.len() == 1))
            {
                let members = fields
                    .into_iter()
                    .filter_map(|(field, items)| match items {
                        ScopeItems::Members(mut items) => Some((field, items.pop()?)),
                        ScopeItems::Positions(_) => None,
                    })
                    .collect();
                Some(AxisSort::Scoped {
                    data,
                    descending,
                    members,
                })
            } else {
                Some(AxisSort::Selection {
                    data,
                    descending,
                    fields,
                })
            }
        }
    }

    fn on(value: Option<String>) -> bool {
        value.is_some_and(|v| matches!(v.as_str(), "1" | "true"))
    }
}
