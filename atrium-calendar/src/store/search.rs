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
                    query.start.pred_opt().unwrap_or(query.start),
                    query.end.succ_opt().unwrap_or(query.end),
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
                    if let Some(zone) = entry.zone {
                        if let Ok((start, _)) = zone.interval(
                            date,
                            entry.span,
                            entry.kind == Kind::Weekly && date != entry.date,
                        ) {
                            let shown = start.date_naive().max(query.start);
                            if shown <= query.end {
                                if let Some(span) = entry.segment(date, shown) {
                                    matches.push((shown, span, entry));
                                }
                            }
                        }
                    } else {
                        matches.push((date, entry.span, entry));
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
        matches.sort_unstable_by_key(|(date, span, e)| (*date, span.start, span.end, e.id));
        let total = matches.len();
        if offset != 0 && offset >= total {
            return Err("Result page is unavailable. Search again.".into());
        }
        let hits = matches
            .into_iter()
            .skip(offset)
            .take(SEARCH_PAGE_SIZE)
            .map(|(date, span, entry)| SearchHit {
                id: entry.id,
                date,
                span,
                source_zone: entry.zone.map(|z| z.name),
                title: entry.title.clone(),
                kind: entry.kind,
            })
            .collect();
        Ok(SearchPage {
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
                .any(|e| e.id == id && e.on_day(date).is_some())
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
