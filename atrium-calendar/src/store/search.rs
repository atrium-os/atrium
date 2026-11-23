//! Read-only occurrence search over one already authorized persona snapshot.
use super::Store;
use crate::{CalendarPage, CalendarView, Entry, Kind, Span};
use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SEARCH_PAGE_SIZE: usize = 5;
const MAX_CANDIDATES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub text: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::SourceSlot>,
    pub id: Uuid,
    pub date: NaiveDate,
    pub span: Span,
    pub title: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_zone: Option<chrono_tz::Tz>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPage {
    pub viewing_zone: chrono_tz::Tz,
    pub revision: u64,
    pub total: usize,
    pub hits: Vec<SearchHit>,
}

impl SearchPage {
    pub(crate) fn validate(
        &self,
        start: NaiveDate,
        end: NaiveDate,
        offset: usize,
        revision: Option<u64>,
    ) -> Result<(), String> {
        if self.total > MAX_CANDIDATES
            || offset > self.total
            || self.hits.len() != (self.total - offset).min(SEARCH_PAGE_SIZE)
            || revision.is_some_and(|r| self.revision != r)
        {
            return Err("Invalid search result page".into());
        }
        for hit in &self.hits {
            crate::validate_event_fields(hit.id, &hit.title, hit.span, hit.kind)?;
            if !(start..=end).contains(&hit.date) {
                return Err("Search result outside requested dates".into());
            }
        }
        Ok(())
    }
}

impl Store {
    pub fn search(
        &self,
        query: &SearchQuery,
        offset: usize,
        revision: Option<u64>,
    ) -> Result<SearchPage, String> {
        if (offset != 0 || revision.is_some()) && revision != Some(self.state.revision) {
            return Err("Calendar changed. Search again.".into());
        }
        if query.text.trim().is_empty()
            || query.text.len() > 160
            || query.text.chars().any(char::is_control)
        {
            return Err("Enter a title or words from a title, up to 160 bytes.".into());
        }
        if !(0..366).contains(&(query.end - query.start).num_days()) {
            return Err("Choose a date range from 1 to 366 days.".into());
        }
        let folded = query.text.to_lowercase();
        let words: Vec<_> = folded.split_whitespace().collect();
        let mut matches = Vec::new();
        let mut candidates = 0;
        for entry in &self.state.entries {
            let title = entry.title.to_lowercase();
            if !words.iter().all(|word| title.contains(word)) {
                continue;
            }
            let (first, last) = if entry.zone.is_some() {
                (
                    crate::zone::nearby(query.start, -2),
                    crate::zone::nearby(query.end, 2),
                )
            } else {
                (query.start, query.end)
            };
            let Some(mut date) = first_candidate(entry, first) else {
                continue;
            };
            while date <= last {
                candidates += 1;
                if candidates > MAX_CANDIDATES {
                    return Err("Too many occurrences. Narrow the dates or title.".into());
                }
                if entry.appears(date) {
                    if entry.zone.is_some() {
                        let row = (-2..=2)
                            .map(|n| crate::zone::nearby(date, n))
                            .filter(|d| (query.start..=query.end).contains(d))
                            .find_map(|shown| entry.project(date, shown, self.viewing_zone()));
                        if let Some(row) = row {
                            matches.push((row.date, row.span, entry, row.source));
                        }
                    } else {
                        matches.push((date, entry.span, entry, None));
                    }
                }
                if entry.kind != Kind::Weekly {
                    break;
                }
                let Some(next) = date.checked_add_days(Days::new(7)) else {
                    break;
                };
                date = next;
            }
        }
        matches.sort_by_cached_key(|(date, span, e, source)| {
            (
                *date,
                crate::zone::sort_time(*source, *span, *date, self.viewing_zone()),
                span.end,
                e.id,
            )
        });
        let total = matches.len();
        if offset != 0 && offset >= total {
            return Err("Result page is unavailable. Search again.".into());
        }
        let hits = matches
            .into_iter()
            .skip(offset)
            .take(SEARCH_PAGE_SIZE)
            .map(|(date, span, entry, source)| SearchHit {
                source,
                id: entry.id,
                date,
                span,
                source_zone: entry.zone.map(|z| z.name),
                title: entry.title.clone(),
                kind: entry.kind,
            })
            .collect();
        Ok(SearchPage {
            viewing_zone: self.viewing_zone(),
            revision: self.state.revision,
            total,
            hits,
        })
    }

    pub fn open_search_hit(
        &self,
        revision: u64,
        id: Uuid,
        date: NaiveDate,
    ) -> Result<CalendarPage, String> {
        if revision != self.state.revision
            || !self
                .state
                .entries
                .iter()
                .any(|e| e.id == id && e.on_day(date, self.viewing_zone()).is_some())
        {
            return Err("Calendar changed. Search again.".into());
        }
        Ok(self.page(date, Some(CalendarView::Today)))
    }
}

// Jump over years of history directly to the first candidate in the window.
fn first_candidate(entry: &Entry, start: NaiveDate) -> Option<NaiveDate> {
    if entry.date >= start {
        return Some(entry.date);
    }
    if entry.kind != Kind::Weekly {
        return None;
    }
    let elapsed = (start - entry.date).num_days() as u64;
    start.checked_add_days(Days::new((7 - elapsed % 7) % 7))
}
