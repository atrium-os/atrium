//! How visible members are ordered within each parent on a pivot axis.

use super::{get_field, get_value, key, put_value, Acc, Agg, PivotField, MAX_FIELDS, MAX_ITEMS};
use crate::value::Value;
use officina_core::wire::{Reader, WireError, Writer};
use std::collections::HashMap;
use std::sync::Arc;

pub const MAX_SCOPE_ITEMS: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum ScopeItems {
    Members(Vec<Value>),
    Positions(Vec<u32>),
}

impl ScopeItems {
    fn len(&self) -> usize {
        match self {
            Self::Members(items) => items.len(),
            Self::Positions(items) => items.len(),
        }
    }
}

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
    /// The zero-based value field and a prefix of the opposite axis. An empty
    /// prefix sorts by the grand total of that value field.
    Scoped {
        data: u32,
        descending: bool,
        members: Vec<(u32, Value)>,
    },
    Selection {
        data: u32,
        descending: bool,
        fields: Vec<(u32, ScopeItems)>,
    },
}

impl AxisSort {
    pub fn is_selected_value_sort(&self) -> bool {
        match self {
            Self::Scoped { members, .. } => !members.is_empty(),
            Self::Selection { .. } => true,
            _ => false,
        }
    }

    pub fn uses_positions(&self) -> bool {
        matches!(self, Self::Selection { fields, .. } if fields.iter().any(|(_, items)| matches!(items, ScopeItems::Positions(_))))
    }

    pub fn uses_values(&self) -> bool {
        matches!(
            self,
            Self::Data { .. } | Self::Scoped { .. } | Self::Selection { .. }
        )
    }

    pub fn scoped_measure(&self) -> Option<u32> {
        match self {
            Self::Scoped { data, .. } | Self::Selection { data, .. } => Some(*data),
            _ => None,
        }
    }

    pub(super) fn scoped_measure_mut(&mut self) -> Option<&mut u32> {
        match self {
            Self::Scoped { data, .. } | Self::Selection { data, .. } => Some(data),
            _ => None,
        }
    }

    pub(super) fn matches_axis(&self, axis: &[PivotField]) -> bool {
        let prefix = |len| axis.iter().take(len).map(|field| field.field);
        match self {
            Self::Scoped { members, .. } => {
                prefix(members.len()).eq(members.iter().map(|(id, _)| *id))
            }
            Self::Selection { fields, .. } => {
                prefix(fields.len()).eq(fields.iter().map(|(id, _)| *id))
            }
            _ => true,
        }
    }

    pub(super) fn valid(&self) -> bool {
        match self {
            Self::Manual(items) => items.len() <= MAX_ITEMS,
            Self::Data { caption, .. } => {
                !caption.is_empty() && caption.len() <= crate::strings::MAX_TEXT
            }
            Self::Scoped { data, members, .. } => {
                (*data as usize) < MAX_FIELDS && members.len() <= MAX_FIELDS
            }
            Self::Selection { data, fields, .. } => {
                (*data as usize) < MAX_FIELDS
                    && !fields.is_empty()
                    && fields.len() <= MAX_FIELDS
                    && fields.iter().all(|(_, items)| items.len() > 0)
                    && fields.iter().map(|(_, items)| items.len()).sum::<usize>() <= MAX_SCOPE_ITEMS
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
            Self::Scoped {
                data,
                descending,
                members,
            } => {
                w.u8(7 + u8::from(*descending));
                w.var(u64::from(*data));
                w.var(members.len() as u64);
                for (field, item) in members {
                    w.var(u64::from(*field));
                    put_value(w, item);
                }
            }
            Self::Selection {
                data,
                descending,
                fields,
            } => {
                w.u8(9 + u8::from(*descending));
                w.var(u64::from(*data));
                w.var(fields.len() as u64);
                for (field, items) in fields {
                    w.var(u64::from(*field));
                    w.u8(u8::from(matches!(items, ScopeItems::Positions(_))));
                    w.var(items.len() as u64);
                    match items {
                        ScopeItems::Members(items) => {
                            for item in items {
                                put_value(w, item);
                            }
                        }
                        ScopeItems::Positions(items) => {
                            for item in items {
                                w.var(u64::from(*item));
                            }
                        }
                    }
                }
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
            tag @ (7 | 8) => {
                let data = r.len("pivot sort value field", (MAX_FIELDS - 1) as u64)? as u32;
                let n = r.count("pivot sort members", MAX_FIELDS as u64, 2)?;
                let mut members = Vec::with_capacity(n);
                for _ in 0..n {
                    members.push((get_field(r)?, get_value(r)?));
                }
                Self::Scoped {
                    data,
                    descending: tag == 8,
                    members,
                }
            }
            tag @ (9 | 10) => {
                let data = r.len("pivot sort value field", (MAX_FIELDS - 1) as u64)? as u32;
                let n = r.count("pivot sort fields", MAX_FIELDS as u64, 3)?;
                let mut fields = Vec::with_capacity(n);
                let mut left = MAX_SCOPE_ITEMS;
                for _ in 0..n {
                    let field = get_field(r)?;
                    let kind = r.u8()?;
                    let n = r.count("pivot sort selections", left as u64, 1)?;
                    left -= n;
                    let items = match kind {
                        0 => ScopeItems::Members(
                            (0..n).map(|_| get_value(r)).collect::<Result<_, _>>()?,
                        ),
                        1 => ScopeItems::Positions(
                            (0..n)
                                .map(|_| {
                                    r.len("pivot sort item position", MAX_ITEMS as u64)
                                        .map(|v| v as u32)
                                })
                                .collect::<Result<_, _>>()?,
                        ),
                        _ => return Err(WireError::Invalid("pivot sort selector")),
                    };
                    fields.push((field, items));
                }
                Self::Selection {
                    data,
                    descending: tag == 10,
                    fields,
                }
            }
            _ => return Err(WireError::Invalid("pivot item sort")),
        })
    }
}

type Path = Vec<u32>;
type Positions = HashMap<Path, HashMap<u32, u32>>;

pub(super) struct SortKey {
    pub data: usize,
    pub agg: Agg,
    pub descending: bool,
    pub opposite: Vec<Path>,
}

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
        resolve: impl Fn(&AxisSort, bool) -> Option<SortKey>,
    ) -> Self {
        let levels = axis
            .iter()
            .enumerate()
            .map(|(level, field)| {
                let SortKey {
                    data,
                    agg,
                    descending,
                    opposite,
                } = resolve(&field.sort, column)?;
                // Merge only present intersections for a multi-item scope.
                let combined = (opposite.len() > 1).then(|| {
                    let selected: std::collections::HashSet<_> = opposite.iter().collect();
                    let mut intersections: Vec<_> = acc
                        .iter()
                        .filter(|((row, col), _)| {
                            let (own, other) = if column { (col, row) } else { (row, col) };
                            own.len() == level + 1 && selected.contains(other)
                        })
                        .collect();
                    intersections.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
                    let mut totals: HashMap<&[u32], Acc> = HashMap::new();
                    for ((row, col), values) in intersections {
                        let own = if column { col } else { row };
                        if let Some(value) = values.get(data) {
                            totals.entry(own.as_slice()).or_default().merge(value);
                        }
                    }
                    totals
                });
                let mut families: HashMap<Path, Vec<(u32, Option<f64>)>> = HashMap::new();
                for path in paths {
                    let item = path[level];
                    let siblings = families.entry(path[..level].to_vec()).or_default();
                    // Paths are visited in item-ID order, so repeats of a child are adjacent.
                    if siblings.last().is_some_and(|(last, _)| *last == item) {
                        continue;
                    }
                    let members = path[..=level].to_vec();
                    let get = |path: &Path| {
                        let target = if column {
                            (path.clone(), members.clone())
                        } else {
                            (members.clone(), path.clone())
                        };
                        acc.get(&target).and_then(|values| values.get(data))
                    };
                    let value = match opposite.as_slice() {
                        [path] => get(path).map(|acc| acc.result(agg)),
                        _ => combined
                            .as_ref()
                            .and_then(|totals| totals.get(members.as_slice()))
                            .map(|acc| acc.result(agg)),
                    };
                    let amount = match value {
                        Some(Value::Num(n)) if n.is_finite() => Some(n),
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
                                        if descending {
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

/// Select disjoint opposite-axis prefixes. Paths arrive in visible label/manual
/// order, with hidden members already removed.
pub(super) fn selected_paths(
    paths: &[Path],
    fields: &[(u32, ScopeItems)],
    lookup: impl Fn(u32, &Value) -> Option<u32>,
) -> Vec<Path> {
    use std::collections::HashSet;
    let mut parents = HashSet::from([Vec::new()]);
    for (level, (field, items)) in fields.iter().enumerate() {
        let wanted: HashSet<u32> = match items {
            ScopeItems::Members(items) => items
                .iter()
                .filter_map(|item| lookup(*field, item))
                .collect(),
            ScopeItems::Positions(items) => items.iter().copied().collect(),
        };
        let mut matched = HashSet::new();
        let mut previous: Option<&[u32]> = None;
        let mut position = 0;
        for path in paths {
            let parent = &path[..level];
            let member = &path[..=level];
            if !parents.contains(parent) || previous == Some(member) {
                continue;
            }
            position = if previous.is_some_and(|prev| &prev[..level] == parent) {
                position + 1
            } else {
                0
            };
            previous = Some(member);
            let id = match items {
                ScopeItems::Members(_) => path[level],
                ScopeItems::Positions(_) => position,
            };
            if wanted.contains(&id) {
                matched.insert(member.to_vec());
            }
        }
        parents = matched;
        if parents.is_empty() {
            break;
        }
    }
    let mut prefixes: Vec<_> = parents.into_iter().collect();
    prefixes.sort_unstable();
    prefixes
}

/// Missing explicitly ordered members still need entries in a foreign definition.
pub(super) fn add_missing(items: &mut Vec<Value>, prior: &[Value]) {
    let mut seen: std::collections::HashSet<_> = items.iter().map(key).collect();
    items.extend(prior.iter().filter(|item| seen.insert(key(item))).cloned());
}
