//! One Fasti process owns one private, supervisor-provisioned persona directory.
mod import;
mod invitation_skips;
mod invitations;
mod ranges;
pub mod search;
use super::{CalendarPage, CalendarView, Day, EditScope, Entry, EventEdit, Kind, UndoReceipt};
use chrono::NaiveDate;
use chrono_tz::Tz;
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
pub const UNDO_EXPIRED: &str = "Undo expired. Refresh to see the current calendar.";

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u8,
    revision: u64,
    #[serde(default)]
    opening_view: CalendarView,
    #[serde(default)]
    pinned_zone: Option<Tz>,
    #[serde(default)]
    device_zone: Option<Tz>,
    entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cancelled: Vec<crate::invitation::Cancellation>,
}

struct UndoState {
    receipt: UndoReceipt,
    date: NaiveDate,
    entries: Vec<Entry>,
    cancelled: Vec<crate::invitation::Cancellation>,
}

pub struct Store {
    preview: Option<import::Pending>,
    invite: Option<invitations::Pending>,
    undo: Option<UndoState>,
    dir: PathBuf,
    _lock: File,
    device_available: bool,
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
                let mut snapshot: Snapshot = serde_json::from_slice(&bytes)?;

                if !matches!(snapshot.version, 1..=9)
                    || snapshot.cancelled.len() > MAX_ENTRIES
                    || validate_records(&snapshot.entries).is_err()
                {
                    return Err(io::Error::other("Invalid calendar snapshot"));
                }
                if snapshot.version == 8 {
                    ranges::upgrade(&mut snapshot.entries).map_err(io::Error::other)?;
                }
                snapshot
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Snapshot {
                version: 1,
                pinned_zone: None,
                device_zone: None,
                revision: 0,
                opening_view: CalendarView::Today,
                entries: Vec::new(),
                cancelled: Vec::new(),
            },
            Err(e) => return Err(e),
        };
        Ok(Self {
            preview: None,
            invite: None,
            undo: None,
            dir: dir.into(),
            _lock: lock,
            device_available: true,
            state,
        })
    }

    pub fn has_pinned_zone(&self) -> bool {
        self.state.pinned_zone.is_some()
    }

    pub fn viewing_zone(&self) -> Tz {
        self.state
            .pinned_zone
            .or(self.state.device_zone)
            .unwrap_or(chrono_tz::UTC)
    }

    pub fn device_unavailable(&mut self) {
        self.device_available = false;
    }

    pub fn sync_device(&mut self, device: Tz) -> Result<(), String> {
        self.device_available = true;
        if self.state.device_zone.is_some_and(|old| old != device)
            && self.state.pinned_zone.is_none()
        {
            return self.commit_zones(self.state.revision, None, Some(device));
        }
        self.state.device_zone = Some(device);
        Ok(())
    }

    pub fn set_viewing_zone(&mut self, revision: u64, pinned: Option<Tz>) -> Result<(), String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        if pinned.is_none() && !self.device_available {
            return Err("Device time zone unavailable".into());
        }
        if self.state.pinned_zone == pinned {
            return Ok(());
        }
        self.commit_zones(revision, pinned, self.state.device_zone)
    }

    fn commit_zones(
        &mut self,
        revision: u64,
        pinned_zone: Option<Tz>,
        device_zone: Option<Tz>,
    ) -> Result<(), String> {
        self.publish(
            revision,
            self.state.entries.clone(),
            self.state.cancelled.clone(),
            self.state.opening_view,
            pinned_zone,
            device_zone,
        )
        .map(drop)
    }

    pub fn day(&self, date: NaiveDate) -> Day {
        self.page(date, Some(CalendarView::Today)).into_day()
    }

    pub fn page(&self, date: NaiveDate, view: Option<CalendarView>) -> CalendarPage {
        let view = view.unwrap_or(self.state.opening_view);
        let dates: Vec<_> = view.dates(date).collect();
        let window = self.window(dates[0], *dates.last().unwrap());
        let mut entries: Vec<_> = window
            .iter()
            .map(|e| e.as_ref())
            .filter_map(|e| {
                if view == CalendarView::Today {
                    e.on_day(date, self.viewing_zone())
                } else if dates.iter().any(|&d| {
                    if e.zone.is_some() {
                        e.on_day(d, self.viewing_zone()).is_some()
                    } else {
                        e.appears(d)
                    }
                }) {
                    Some(e.clone())
                } else {
                    None
                }
            })
            .collect();
        entries.sort_by_cached_key(|e| {
            (
                crate::zone::sort_time(e.source, e.span, date, self.viewing_zone()),
                e.span.end,
                e.id,
            )
        });
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
            entry.excluded.retain(|date| {
                *date >= crate::zone::nearby(dates[0], -2)
                    && *date <= crate::zone::nearby(*dates.last().unwrap(), 2)
            });
        }
        CalendarPage {
            undo: None,
            clock_gap: crate::zone::missing_minutes(date, self.viewing_zone()),
            date,
            viewing_zone: self.viewing_zone(),
            pinned_zone: self.state.pinned_zone,
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
        self.publish(
            revision,
            self.state.entries.clone(),
            self.state.cancelled.clone(),
            view,
            self.state.pinned_zone,
            self.state.device_zone,
        )
        .map(drop)
    }

    pub fn add(&mut self, revision: u64, event: Entry) -> Result<UndoReceipt, String> {
        event.validate()?;
        if event.invitation.is_some()
            || event.import_hash.is_some()
            || event.source.is_some()
            || !event.excluded.is_empty()
            || matches!(event.kind, Kind::Occurrence { .. })
        {
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
            && crate::zone::missing_minutes(event.date, self.viewing_zone())
                .is_some_and(|gap| gap.overlaps(event.span))
        {
            return Err("That time does not exist in the viewing zone".into());
        }
        if event.kind == Kind::Hold
            && self
                .day(event.date)
                .entries
                .iter()
                .any(|e| e.busy_spans().any(|s| s.overlaps(event.span)))
        {
            return Err("That time is already busy".into());
        }
        let mut entries = self.state.entries.clone();
        let date = event
            .fields_on(event.date)
            .agenda_date(self.viewing_zone())?;
        entries.push(event);
        self.commit_event(revision, entries, date)
    }

    pub fn release(
        &mut self,
        revision: u64,
        date: NaiveDate,
        id: uuid::Uuid,
    ) -> Result<UndoReceipt, String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        let mut entries = self.state.entries.clone();
        let index = entries
            .iter()
            .position(|e| e.id == id && e.date == date && e.kind == Kind::Hold)
            .ok_or("Hold no longer exists")?;
        entries.remove(index);
        self.commit_event(revision, entries, date)
    }

    /// Both the weekly exclusion and its replacement are published in one revision.
    pub fn change(
        &mut self,
        revision: u64,
        id: Uuid,
        shown: NaiveDate,
        scope: EditScope,
        replacement: Option<EventEdit>,
    ) -> Result<Option<UndoReceipt>, String> {
        if revision != self.state.revision {
            return Err("Calendar changed. Refresh before saving.".into());
        }
        let mut entries = self.state.entries.clone();
        let window = self.window(shown, shown);
        let visible = window
            .iter()
            .find(|e| e.id == id && e.on_day(shown, self.viewing_zone()).is_some())
            .ok_or("Event is no longer on this day")?;
        let target = if scope == EditScope::WholeSeries {
            visible.series_id().ok_or("Select a weekly series")?
        } else {
            id
        };
        let selected = if let Some(i) = entries.iter().position(|e| e.id == target) {
            i
        } else {
            let Kind::Occurrence { series, original } = visible.kind else {
                return Err("Missing weekly occurrence".into());
            };
            let parent = entries
                .iter_mut()
                .find(|e| e.id == series)
                .ok_or("Missing weekly series")?;
            let at = parent
                .excluded
                .binary_search(&original)
                .err()
                .ok_or("This occurrence changed. Refresh the calendar.")?;
            parent.excluded.insert(at, original);
            entries.push(visible.as_ref().clone());
            entries.len() - 1
        };
        let agenda_date = shown;
        let shown = visible
            .on_day(shown, self.viewing_zone())
            .and_then(|e| e.source)
            .map_or(shown, |s| s.date);
        let kind = entries[selected].kind;
        if kind == Kind::Hold {
            return Err("Release the hold from the agenda".into());
        }
        if scope == EditScope::ThisEvent
            && replacement.as_ref() == Some(&entries[selected].fields_on(shown))
        {
            return Ok(None);
        }
        let mut index = selected;
        if scope == EditScope::WholeSeries {
            let series = target;
            if let Some(fields) = &replacement {
                if *fields == entries[index].fields_on(entries[index].date) {
                    return Ok(None);
                }
                if fields.date != entries[index].date {
                    return Err("Keep the series start date when changing its weekly time".into());
                }
            } else {
                entries.retain(|e| e.series_id() != Some(series));
                return self.commit_event(revision, entries, agenda_date).map(Some);
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
                if let Some(origin) = &mut occurrence.invitation {
                    origin.ranges.clear();
                    origin.range_sequence = None;
                }
                index = entries.len();
                entries.push(occurrence);
            } else {
                return self.commit_event(revision, entries, agenda_date).map(Some);
            }
        }
        if let Some(fields) = replacement {
            entries[index].date = fields.date;
            entries[index].title = fields.title;
            entries[index].span = fields.span;
            entries[index].zone = fields.zone;
            if entries[index].kind == Kind::Weekly {
                if let Some(rule) = entries[index].invitation.as_ref().and_then(|o| o.limit) {
                    let last = rule.last(&entries[index])?;
                    entries[index].invitation.as_mut().unwrap().last = Some(last);
                }
            }
        } else {
            entries.remove(index);
        }
        self.commit_event(revision, entries, agenda_date).map(Some)
    }

    fn commit_event(
        &mut self,
        revision: u64,
        entries: Vec<Entry>,
        date: NaiveDate,
    ) -> Result<UndoReceipt, String> {
        self.commit_invitation(revision, entries, self.state.cancelled.clone(), date)
    }

    fn commit_invitation(
        &mut self,
        revision: u64,
        entries: Vec<Entry>,
        cancelled: Vec<crate::invitation::Cancellation>,
        date: NaiveDate,
    ) -> Result<UndoReceipt, String> {
        let previous = self.publish(
            revision,
            entries,
            cancelled,
            self.state.opening_view,
            self.state.pinned_zone,
            self.state.device_zone,
        )?;
        let receipt = UndoReceipt {
            revision: self.state.revision,
            token: Uuid::new_v4(),
        };
        self.undo = Some(UndoState {
            receipt,
            date,
            entries: previous.entries,
            cancelled: previous.cancelled,
        });
        Ok(receipt)
    }

    pub fn undo(&mut self, receipt: UndoReceipt) -> Result<NaiveDate, String> {
        let previous = self
            .undo
            .as_ref()
            .filter(|u| u.receipt == receipt && self.state.revision == receipt.revision)
            .ok_or(UNDO_EXPIRED)?;
        let date = previous.date;
        // Keep the receipt usable if staging fails. Publishing consumes it at rename.
        self.publish(
            receipt.revision,
            previous.entries.clone(),
            previous.cancelled.clone(),
            self.state.opening_view,
            self.state.pinned_zone,
            self.state.device_zone,
        )?;
        Ok(date)
    }

    fn publish(
        &mut self,
        revision: u64,
        entries: Vec<Entry>,
        cancelled: Vec<crate::invitation::Cancellation>,
        opening_view: CalendarView,
        pinned_zone: Option<Tz>,
        device_zone: Option<Tz>,
    ) -> Result<Snapshot, String> {
        if cancelled.len() > MAX_ENTRIES {
            return Err("Calendar cancellation history limit reached".into());
        }
        validate_records(&entries)?;
        let next = Snapshot {
            opening_view,
            version: 9,
            cancelled,
            pinned_zone,
            device_zone,
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
        let previous = std::mem::replace(&mut self.state, next);
        self.preview = None;
        self.invite = None;
        self.undo = None;
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(|_| {
                "Saved, but durability is uncertain. Refresh before retrying.".to_string()
            })?;
        Ok(previous)
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
        if e.source.is_some() {
            return Err("A projected row cannot be stored");
        }
        e.validate()?;
        if let Some(origin) = &e.invitation {
            if !origin
                .ranges
                .windows(2)
                .all(|pair| pair[0].anchor < pair[1].anchor)
                || origin.ranges.iter().any(|r| {
                    e.kind != Kind::Weekly
                        || !e.scheduled_on(r.anchor)
                        || !r.schedule.span.valid()
                        || r.schedule.zone.is_some() != e.zone.is_some()
                        || r.occurrence(e, r.anchor).is_err()
                })
            {
                return Err("Invalid saved weekly range");
            }
        }
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
