//! The Where field's logic (docs/spec/navigator-ui.md §5.3): what Enter
//! does with what was typed, and the suggestions, from local sources only.
//! Nothing here touches the network: the first byte leaves the machine
//! when Enter sends a search or an address (D4).

use crate::trail::{PageId, TrailStore};

/// What Enter does with the text.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// Go to this address (`https:` assumed when none was typed).
    Address(String),
    /// Search for this, with the chosen provider.
    Search(String),
}

/// Parse what was typed: something that reads as an address goes there,
/// anything else is a search (§5.3).
pub fn parse(input: &str) -> Option<Entry> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(char::is_whitespace) {
        return Some(Entry::Search(s.into()));
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Some(Entry::Address(s.into()));
    }
    let host_end = s.find(['/', '?', '#']).unwrap_or(s.len());
    let host = &s[..host_end];
    let host_only = host.rsplit_once(':').map_or(host, |(h, port)| {
        if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) { h } else { host }
    });
    if looks_like_host(host_only) {
        return Some(Entry::Address(format!("https://{s}")));
    }
    Some(Entry::Search(s.into()))
}

fn looks_like_host(h: &str) -> bool {
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return false;
    }
    if labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return labels.len() == 4; // a dotted IPv4 address
    }
    let tld = labels.last().unwrap();
    labels.iter().all(|l| l.chars().all(|c| c.is_alphanumeric() || c == '-'))
        && tld.chars().count() >= 2
        && tld.chars().all(char::is_alphabetic)
}

/// Where a suggestion comes from, in the order they are listed (§5.3).
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A page already open in a trail: switching to it, not opening again.
    Open { trail: usize, page: PageId, trail_title: String },
    Kept,
    History,
    /// "Search for …", always the last row.
    Search,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub source: Source,
    pub title: String,
    pub url: String,
}

/// At most this many rows above the search row.
pub const MAX_ROWS: usize = 6;

fn matches(q: &str, title: &str, url: &str) -> bool {
    let q = q.to_lowercase();
    title.to_lowercase().contains(&q) || url.split_once("://").map_or(url, |(_, r)| r).to_lowercase().contains(&q)
}

/// The suggestions for `query`: open trails' pages, kept pages, history
/// (pages of trails put away), then the search. One row per address; the
/// earlier source wins.
pub fn suggest(store: &TrailStore, query: &str) -> Vec<Suggestion> {
    let q = query.trim();
    if q.is_empty() {
        return vec![];
    }
    let mut out: Vec<Suggestion> = vec![];
    let push = |out: &mut Vec<Suggestion>, s: Suggestion| {
        if out.len() < MAX_ROWS && !out.iter().any(|o| o.url == s.url) {
            out.push(s);
        }
    };
    for ti in store.rail_order() {
        let t = &store.trails[ti];
        for (pi, p) in t.pages.iter().enumerate() {
            if p.search.is_none() && matches(q, &p.title, &p.url) && !(ti == store.current && t.current == Some(pi)) {
                push(&mut out, Suggestion {
                    source: Source::Open { trail: ti, page: pi, trail_title: t.title().into() },
                    title: p.title.clone(),
                    url: p.url.clone(),
                });
            }
        }
    }
    for k in &store.kept {
        if matches(q, &k.title, &k.url) {
            push(&mut out, Suggestion { source: Source::Kept, title: k.title.clone(), url: k.url.clone() });
        }
    }
    for e in &store.earlier {
        for p in &e.trail.pages {
            if p.search.is_none() && matches(q, &p.title, &p.url) {
                push(&mut out, Suggestion { source: Source::History, title: p.title.clone(), url: p.url.clone() });
            }
        }
    }
    out.push(Suggestion { source: Source::Search, title: q.into(), url: String::new() });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trail::{Facts, Media, PageSpec, Trail};

    #[test]
    fn parse_addresses_and_searches() {
        assert_eq!(parse("  "), None);
        assert_eq!(parse("atlas.example/wiki"), Some(Entry::Address("https://atlas.example/wiki".into())));
        assert_eq!(parse("http://old.example"), Some(Entry::Address("http://old.example".into())));
        assert_eq!(parse("localhost:8080/x"), Some(Entry::Address("https://localhost:8080/x".into())));
        assert_eq!(parse("10.0.0.1"), Some(Entry::Address("https://10.0.0.1".into())));
        assert_eq!(parse("roman atrium"), Some(Entry::Search("roman atrium".into())));
        assert_eq!(parse("impl"), Some(Entry::Search("impl".into())));
        assert_eq!(parse("3.14"), Some(Entry::Search("3.14".into())));
        assert_eq!(parse("file.rs"), Some(Entry::Address("https://file.rs".into())));
    }

    fn spec(url: &str, title: &str) -> PageSpec {
        PageSpec { url: url.into(), title: title.into(), search: None, facts: Facts::green(""), media: Media::None }
    }

    #[test]
    fn suggestions_are_local_and_ordered() {
        let mut s = TrailStore::empty();
        let mut t = Trail::new(0, false);
        t.follow(spec("https://atlas.example/wiki/Atrium", "Atrium"));
        t.follow(spec("https://atlas.example/wiki/Impluvium", "Impluvium"));
        t.back();
        s.add(t);
        s.kept.push(crate::trail::Kept { url: "https://doc.rust.example/iter".into(), title: "Implementing Iterator".into() });
        s.kept.push(crate::trail::Kept { url: "https://atlas.example/wiki/Impluvium".into(), title: "Impluvium".into() });
        let mut old = Trail::new(0, false);
        old.follow(spec("https://blog.rust.example/impl-trait", "impl Trait"));
        s.earlier.push(crate::trail::PutAway { trail: old, day: "Wed".into(), at: String::new() });
        s.current = 0;

        let v = suggest(&s, "impl");
        let kinds: Vec<_> = v.iter().map(|x| std::mem::discriminant(&x.source)).collect();
        assert!(matches!(v[0].source, Source::Open { page: 1, .. }), "an open page first: {v:?}");
        assert!(matches!(v[1].source, Source::Kept));
        assert_eq!(v.iter().filter(|x| x.url.ends_with("Impluvium")).count(), 1, "one row per address");
        assert!(matches!(v[2].source, Source::History));
        assert!(matches!(v.last().unwrap().source, Source::Search));
        assert_eq!(kinds.len(), 4);
        // The page on screen is not suggested to itself.
        assert!(suggest(&s, "atrium").iter().all(|x| !x.url.ends_with("/Atrium")));
        assert!(suggest(&s, "").is_empty());
    }
}
