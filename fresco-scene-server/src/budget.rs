//! What a client's scene costs its renderer, and the client's budget
//! (docs/spec/fresco-f4-split.md §F4h; officina.md §10.5).
//!
//! Every client connection is served by its own `fresco-renderer`, jailed
//! with a 512 MiB memory cap. Before this budget an app that grew its scene
//! without bound ended when the renderer passed that cap and was killed,
//! which to the user looks like a crash. Now the frontend counts what each
//! client holds (nodes, node-parameter bytes, bound texture bytes, glyph
//! atlas pages) and refuses the op that would pass a limit: the op is not
//! applied and the client gets `EV_BUDGET_REFUSED`, so it can evict and send
//! again. The limits leave room under the cap for the windows' pixels.
//!
//! The counts are kept per window in a [`Ledger`] keyed by node (or slot)
//! id, so whatever removes a node (a clear, a group's subtree going, a
//! client disconnecting, the window closing) gives its share back.

use std::collections::HashMap;

use fresco_protocol::budget;

/// The limits, per client connection, across all its windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub nodes: u64,
    pub scene_bytes: u64,
    pub texture_bytes: u64,
    pub atlas_pages: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            nodes: budget::MAX_NODES,
            scene_bytes: budget::MAX_SCENE_BYTES,
            texture_bytes: budget::MAX_TEXTURE_BYTES,
            atlas_pages: budget::MAX_ATLAS_PAGES,
        }
    }
}

/// What one client holds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub nodes: u64,
    pub scene_bytes: u64,
    pub texture_bytes: u64,
}

/// Per id (node or slot): its writer and its cost in bytes; and the totals
/// per writer.
#[derive(Debug, Default)]
pub struct Ledger {
    entries: HashMap<u32, (u8, u64)>,
    by_writer: HashMap<u8, (u64, u64)>,
}

impl Ledger {
    /// `id` now costs `bytes`, written by `writer` (replacing what it cost
    /// before, whoever wrote it).
    pub fn charge(&mut self, id: u32, writer: u8, bytes: u64) {
        self.release(id);
        self.entries.insert(id, (writer, bytes));
        let t = self.by_writer.entry(writer).or_default();
        t.0 += 1;
        t.1 += bytes;
    }

    /// `id` is gone.
    pub fn release(&mut self, id: u32) {
        if let Some((w, b)) = self.entries.remove(&id) {
            if let Some(t) = self.by_writer.get_mut(&w) {
                t.0 = t.0.saturating_sub(1);
                t.1 = t.1.saturating_sub(b);
                if t.0 == 0 {
                    self.by_writer.remove(&w);
                }
            }
        }
    }

    /// What `id` costs and who wrote it.
    pub fn get(&self, id: u32) -> Option<(u8, u64)> {
        self.entries.get(&id).copied()
    }

    /// `writer`'s count and bytes here.
    pub fn of(&self, writer: u8) -> (u64, u64) {
        self.by_writer.get(&writer).copied().unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What a node costs in scene bytes: its parameters as sent.
pub fn node_bytes(params_len: usize) -> u64 {
    params_len as u64
}

/// What a bound texture costs: its texels times their size.
pub fn texture_bytes(desc: &fresco_protocol::TextureDesc) -> u64 {
    let bpp = match desc.format {
        fresco_protocol::TextureFormat::Rgba8UnormSrgb => 4,
        fresco_protocol::TextureFormat::R8Unorm => 1,
    };
    desc.width as u64 * desc.height as u64 * bpp
}

/// What a glyph run costs in scene bytes (it is shaped here, so its size is
/// that of the run, not of the text sent).
pub fn glyph_run_bytes(glyphs: usize) -> u64 {
    64 + glyphs as u64 * std::mem::size_of::<fresco_protocol::GlyphInstance>() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_counts_per_writer_and_gives_back() {
        let mut l = Ledger::default();
        l.charge(1, 7, 100);
        l.charge(2, 7, 50);
        l.charge(3, 8, 10);
        assert_eq!(l.of(7), (2, 150));
        assert_eq!(l.of(8), (1, 10));
        // Replacing keeps the count and moves the bytes.
        l.charge(1, 7, 20);
        assert_eq!(l.of(7), (2, 70));
        // Another writer takes an id over.
        l.charge(3, 7, 5);
        assert_eq!(l.of(7), (3, 75));
        assert_eq!(l.of(8), (0, 0));
        l.release(1);
        l.release(1);
        assert_eq!(l.of(7), (2, 55));
        assert_eq!(l.len(), 2);
    }
}
