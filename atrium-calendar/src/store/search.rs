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
            let Some(mut date) = first_candidate(entry, query.start) else {
                continue;
            };
            while date <= query.end {
                candidates += 1;
                if candidates > MAX_CANDIDATES {
                    return Err("Too many occurrences. Narrow the dates or title.".into());
                }
                if entry.appears(date) {
                    matches.push((date, entry));
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
        matches.sort_unstable_by_key(|(date, e)| (*date, e.span.start, e.span.end, e.id));
        let total = matches.len();
        if offset != 0 && offset >= total {
            return Err("Result page is unavailable. Search again.".into());
        }
        let hits = matches
            .into_iter()
            .skip(offset)
            .take(SEARCH_PAGE_SIZE)
            .map(|(date, entry)| SearchHit {
                id: entry.id,
                date,
                span: entry.span,
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
                .any(|e| e.id == id && e.appears(date))
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
