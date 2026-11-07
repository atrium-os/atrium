//! ★ A DOCUMENT'S VIDEO SLOTS (limen.md §2.1). The document worker writes
//! one `embed` leaf per `<video>` into the band group it is laid out in
//! (`navigator_render::nsg::embeds`); this keeps the set of slots the
//! resident bands hold, by key, as the UI must place them:
//!
//! - before Limen L6 the Navigator UI owns the `ScrollView`: for each entry
//!   it emits a transparent node at the document box inside the scroll
//!   content and calls `Ctx::slot(id, key)` on it, and Pergola maps and clips
//!   (`pergola::slot::window_rect`; partly clipped releases);
//! - after L6 the doc-viewer child sends the parent the same list as its
//!   `Embeds` message (at most [`EMBEDS_MAX`]).
//!
//! A slot leaves with the last resident band that holds it: a band's group
//! cleared (evicted) drops its leaves, and a slot no group holds is gone.

use navigator_render::source::GroupId;
use navigator_render::EmbedSource;
use std::collections::{BTreeMap, BTreeSet};

/// ★ The top bit of every media slot key, so a video's key never clashes
/// with a credential field's in the same window (Pergola, 2026-11-06).
pub const MEDIA_KEY_BIT: u64 = 1 << 63;
/// Most entries in a doc-viewer `Embeds` message (limen.md §2.1).
pub const EMBEDS_MAX: usize = 64;

/// One slot, in document space (1/64 px of the layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The worker's embed key with [`MEDIA_KEY_BIT`] set.
    pub key: u64,
    pub rect: (i64, i64, i64, i64),
    pub role: String,
    pub src: EmbedSource,
    /// `navigator_render::embed_flags`.
    pub flags: u8,
    /// Its `<track>` subtitles and captions.
    pub tracks: Vec<navigator_render::TextTrack>,
}

/// What changed since the last [`Slots::take_changes`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    pub added: Vec<u64>,
    pub moved: Vec<u64>,
    pub removed: Vec<u64>,
}

#[derive(Default)]
pub struct Slots {
    /// Which keys each resident group holds.
    by_group: BTreeMap<GroupId, BTreeSet<u64>>,
    entries: BTreeMap<u64, Entry>,
    changes: Changes,
}

impl Slots {
    pub fn new() -> Self { Self::default() }

    /// A group was written (`SceneSink::put`, `Event::Bands`' puts).
    pub fn put(&mut self, group: GroupId, nsg: &str) {
        let mut now = BTreeSet::new();
        for slot in navigator_render::nsg::embeds(nsg) {
            let key = slot.key as u64 | MEDIA_KEY_BIT;
            now.insert(key);
            let e = Entry { key, rect: slot.rect, role: slot.role, src: slot.source, flags: slot.flags, tracks: slot.tracks };
            match self.entries.get(&key) {
                None => { self.changes.added.push(key); self.entries.insert(key, e); }
                Some(old) if old.rect != e.rect => { self.changes.moved.push(key); self.entries.insert(key, e); }
                Some(_) => {}
            }
        }
        let old = self.by_group.insert(group, now).unwrap_or_default();
        for k in old { self.drop_if_unheld(k) }
    }

    /// A group was cleared: its band evicted, or the overlay emptied.
    pub fn clear(&mut self, group: GroupId) {
        for k in self.by_group.remove(&group).unwrap_or_default() { self.drop_if_unheld(k) }
    }

    fn drop_if_unheld(&mut self, k: u64) {
        if self.by_group.values().any(|s| s.contains(&k)) { return }
        if self.entries.remove(&k).is_some() {
            self.changes.added.retain(|a| *a != k);
            self.changes.moved.retain(|a| *a != k);
            self.changes.removed.push(k);
        }
    }

    /// Every slot the resident bands hold, in key order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> { self.entries.values() }
    pub fn get(&self, key: u64) -> Option<&Entry> { self.entries.get(&key) }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    pub fn take_changes(&mut self) -> Changes { std::mem::take(&mut self.changes) }

    /// ★ The doc-viewer child's `Embeds` list (after L6): the slots in view
    /// of `view` (`[top, bottom)`, document space), each with its rect in the
    /// child's window px — `None` when partly or wholly out (released) —
    /// at most [`EMBEDS_MAX`], nearest the view first.
    pub fn embeds_message(&self, view: (i64, i64), zoom_64: u32, window_top_px: f64) -> Vec<(u64, Option<[f32; 4]>, String, EmbedSource)> {
        let z = zoom_64 as f64 / 64.0;
        let mut v: Vec<&Entry> = self.entries.values().collect();
        let dist = |e: &Entry| if e.rect.1 + e.rect.3 <= view.0 { view.0 - (e.rect.1 + e.rect.3) } else if e.rect.1 >= view.1 { e.rect.1 - view.1 } else { 0 };
        v.sort_by_key(|e| (dist(e), e.key));
        v.into_iter().take(EMBEDS_MAX).map(|e| {
            let whole = e.rect.1 >= view.0 && e.rect.1 + e.rect.3 <= view.1;
            let rect = whole.then(|| [(e.rect.0 as f64 / 64.0 * z) as f32, (window_top_px + (e.rect.1 - view.0) as f64 / 64.0 * z) as f32,
                                     (e.rect.2 as f64 / 64.0 * z) as f32, (e.rect.3 as f64 / 64.0 * z) as f32]);
            (e.key, rect, e.role.clone(), e.src.clone())
        }).collect()
    }
}
