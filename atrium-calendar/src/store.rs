//! One Fasti process owns one private, supervisor-provisioned persona directory.
use super::{CalendarPage, CalendarView, Day, EditScope, Entry, EventEdit, Kind};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

const MAX_ENTRIES: usize = 1024;
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u8,
    revision: u64,
    #[serde(default)]
    opening_view: CalendarView,
    entries: Vec<Entry>,
}

pub struct Store {
    dir: PathBuf,
    _lock: File,
    state: Snapshot,
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("writer.lock"))?;
        lock.try_lock()?;
        let state = match File::open(dir.join("calendar.json")) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(io::Error::other("Calendar snapshot too large"));
                }
                let snapshot: Snapshot = serde_json::from_slice(&bytes)?;

                if !matches!(snapshot.version, 1 | 2)
                    || validate_records(&snapshot.entries).is_err()
                {
                    return Err(io::Error::other("Invalid calendar snapshot"));
                }
                snapshot
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Snapshot {
                version: 1,
                revision: 0,
                opening_view: CalendarView::Today,
                entries: Vec::new(),
            },
            Err(e) => return Err(e),
        };
        Ok(Self {
            dir: dir.into(),
            _lock: lock,
            state,
        })
    }

    pub fn day(&self, date: NaiveDate) -> Day {
        self.page(date, Some(CalendarView::Today)).into_day()
    }

    pub fn page(&self, date: NaiveDate, view: Option<CalendarView>) -> CalendarPage {
        let view = view.unwrap_or(self.state.opening_view);
        let dates: Vec<_> = view.dates(date).collect();
        let mut entries: Vec<_> = self
            .state
            .entries
            .iter()
            .filter(|e| dates.iter().any(|&d| e.appears(d)))
            .cloned()
            .collect();
        entries.sort_by_key(|e| (e.span.start, e.span.end, e.id));
        let visible: HashSet<_> = entries.iter().map(|e| e.id).collect();
        let missing: HashSet<_> = entries
            .iter()
            .filter_map(Entry::series_id)
            .filter(|id| !visible.contains(id))
            .collect();
        let series = self
            .state
            .entries
            .iter()
            .filter(|e| missing.contains(&e.id))
            .map(|e| (e.id, e.fields_on(e.date)))
            .collect();
        for entry in &mut entries {
            entry
                .excluded
                .retain(|date| dates.binary_search(date).is_ok());
        }
        CalendarPage {
            series,
            view,
            opening_view: self.state.opening_view,
            revision: self.state.revision,
            entries,
        }
    }

    pub fn set_opening_view(&mut self, revision: u64, view: CalendarView) -> Result<(), String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        if view == self.state.opening_view {
            return Ok(());
        }
        self.commit(revision, self.state.entries.clone(), view)
    }

    pub fn add(&mut self, revision: u64, event: Entry) -> Result<(), String> {
        event.validate()?;
        if !event.excluded.is_empty() || matches!(event.kind, Kind::Occurrence { .. }) {
            return Err("Use the occurrence editor to change a series".into());
        }
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        if self.state.entries.len() >= MAX_ENTRIES {
            return Err("Calendar entry limit reached".into());
        }
        if self.state.entries.iter().any(|e| e.id == event.id) {
            return Err("Entry already exists".into());
        }
        if event.kind == Kind::Hold
            && self
                .state
                .entries
                .iter()
                .any(|e| e.appears(event.date) && e.span.overlaps(event.span))
        {
            return Err("That time is already busy".into());
        }
        let mut entries = self.state.entries.clone();
        entries.push(event);
        self.commit(revision, entries, self.state.opening_view)
    }

    pub fn release(
        &mut self,
        revision: u64,
        date: NaiveDate,
        id: uuid::Uuid,
    ) -> Result<(), String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        let mut entries = self.state.entries.clone();
        let index = entries
            .iter()
            .position(|e| e.id == id && e.date == date && e.kind == Kind::Hold)
            .ok_or("Hold no longer exists")?;
        entries.remove(index);
        self.commit(revision, entries, self.state.opening_view)
    }

    /// Both the weekly exclusion and its replacement are published in one revision.
    pub fn change(
        &mut self,
        revision: u64,
        id: Uuid,
        shown: NaiveDate,
        scope: EditScope,
        replacement: Option<EventEdit>,
    ) -> Result<(), String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        let mut entries = self.state.entries.clone();
        let selected = entries
            .iter()
            .position(|e| e.id == id && e.appears(shown))
            .ok_or("Event is no longer on this day")?;
        let kind = entries[selected].kind;
        if kind == Kind::Hold {
            return Err("Release the hold from the agenda".into());
        }
        if scope == EditScope::ThisEvent
            && replacement.as_ref() == Some(&entries[selected].fields_on(shown))
        {
            return Ok(());
        }
        let mut index = selected;
        if scope == EditScope::WholeSeries {
            let series = entries[selected]
                .series_id()
                .ok_or("Select a weekly series")?;
            index = entries
                .iter()
                .position(|e| e.id == series)
                .ok_or("Series is missing")?;
            if let Some(fields) = &replacement {
                if *fields == entries[index].fields_on(entries[index].date) {
                    return Ok(());
                }
                if fields.date != entries[index].date {
                    return Err("Keep the series start date when changing its weekly time".into());
                }
            } else {
                entries.retain(|e| e.series_id() != Some(series));
                return self.commit(revision, entries, self.state.opening_view);
            }
        } else if kind == Kind::Weekly {
            let excluded = &mut entries[index].excluded;
            let pos = excluded.partition_point(|d| *d < shown);
            excluded.insert(pos, shown);
            if replacement.is_some() {
                let mut occurrence = entries[index].clone();
                occurrence.id = Uuid::new_v4();
                occurrence.kind = Kind::Occurrence {
                    series: id,
                    original: shown,
                };
                occurrence.excluded.clear();
                index = entries.len();
                entries.push(occurrence);
            } else {
                return self.commit(revision, entries, self.state.opening_view);
            }
        }
        if let Some(fields) = replacement {
            entries[index].date = fields.date;
            entries[index].title = fields.title;
            entries[index].span = fields.span;
        } else {
            entries.remove(index);
        }
        self.commit(revision, entries, self.state.opening_view)
    }

    fn commit(
        &mut self,
        revision: u64,
        entries: Vec<Entry>,
        opening_view: CalendarView,
    ) -> Result<(), String> {
        validate_records(&entries)?;
        let next = Snapshot {
            opening_view,
            version: 2,
            revision: revision.checked_add(1).ok_or("Revision limit reached")?,
            entries,
        };
        let bytes = serde_json::to_vec(&next).map_err(|_| "Cannot encode calendar")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Calendar storage limit reached".into());
        }
        let mut tmp = NamedTempFile::new_in(&self.dir).map_err(|_| "Cannot stage calendar")?;
        tmp.write_all(&bytes)
            .and_then(|_| tmp.as_file().sync_all())
            .map_err(|_| "Cannot write calendar")?;
        tmp.persist(self.dir.join("calendar.json"))
            .map_err(|_| "Cannot publish calendar")?;
        self.state = next;
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(|_| "Saved, but durability is uncertain. Refresh before retrying.".to_string())
    }
}

// Validate persisted relationships as well as individual rows before publication.
fn validate_records(entries: &[Entry]) -> Result<(), &'static str> {
    if entries.len() > MAX_ENTRIES
        || entries.iter().map(|e| e.excluded.len()).sum::<usize>() > MAX_ENTRIES
    {
        return Err("Calendar entry or recurrence exception limit reached");
    }
    let mut by_id = BTreeMap::new();
    for e in entries {
        e.validate()?;
        if by_id.insert(e.id, e).is_some() {
            return Err("Duplicate event identity");
        }
        if !e.excluded.is_empty()
            && (e.kind != Kind::Weekly
                || !e.excluded.windows(2).all(|pair| pair[0] < pair[1])
                || !e.excluded.iter().all(|d| e.scheduled_on(*d)))
        {
            return Err("Invalid series exclusions");
        }
    }
    let mut overrides = HashSet::new();
    for e in entries {
        if let Kind::Occurrence { series, original } = e.kind {
            let parent = by_id.get(&series).ok_or("Missing weekly series")?;
            if parent.excluded.binary_search(&original).is_err()
                || !overrides.insert((series, original))
            {
                return Err("Invalid weekly occurrence");
            }
        }
    }
    Ok(())
}
