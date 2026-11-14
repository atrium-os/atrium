//! The block tree (docs/spec/officina.md §7.4): blocks in leaves of at most
//! [`LEAF`] blocks, with a Fenwick index over the leaves' counts, so "block i",
//! "the block holding character c" and an insert or edit are O(log n) plus the
//! work inside one leaf, at any document length.

use crate::model::Block;

/// Blocks per leaf, at most; a fuller leaf splits.
pub const LEAF: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub blocks: u64,
    /// Characters, each block's end counting as one (as a paragraph mark does).
    pub chars: u64,
}

fn count(b: &Block) -> Counts {
    match b {
        Block::Para(p) => Counts { blocks: 1, chars: p.text.chars().count() as u64 + 1 },
    }
}

#[derive(Debug, Clone, Default)]
struct Leaf {
    blocks: Vec<Block>,
    counts: Counts,
}

impl Leaf {
    fn recount(&mut self) {
        let mut c = Counts::default();
        for b in &self.blocks {
            let k = count(b);
            c.blocks += k.blocks;
            c.chars += k.chars;
        }
        self.counts = c;
    }
}

/// Fenwick tree over leaf counts.
#[derive(Debug, Clone, Default)]
struct Fenwick {
    t: Vec<Counts>,
}

impl Fenwick {
    fn build(leaves: &[Leaf]) -> Fenwick {
        let n = leaves.len();
        let mut t = vec![Counts::default(); n + 1];
        for (i, l) in leaves.iter().enumerate() {
            let k = i + 1;
            t[k].blocks += l.counts.blocks;
            t[k].chars += l.counts.chars;
            let j = k + (k & k.wrapping_neg());
            if j <= n {
                let (b, c) = (t[k].blocks, t[k].chars);
                t[j].blocks += b;
                t[j].chars += c;
            }
        }
        Fenwick { t }
    }
    fn add(&mut self, leaf: usize, blocks: i64, chars: i64) {
        let mut k = leaf + 1;
        while k < self.t.len() {
            self.t[k].blocks = (self.t[k].blocks as i64 + blocks) as u64;
            self.t[k].chars = (self.t[k].chars as i64 + chars) as u64;
            k += k & k.wrapping_neg();
        }
    }
    /// Sum of leaves before `leaf`.
    fn prefix(&self, leaf: usize) -> Counts {
        let mut c = Counts::default();
        let mut k = leaf;
        while k > 0 {
            c.blocks += self.t[k].blocks;
            c.chars += self.t[k].chars;
            k &= k - 1;
        }
        c
    }
    /// The leaf where the running total of `key` passes `target`, and the total
    /// before it. `key` picks blocks or chars.
    fn search(&self, target: u64, key: impl Fn(&Counts) -> u64) -> (usize, u64) {
        let Some(n) = self.t.len().checked_sub(1) else { return (0, 0) };
        let mut pos = 0usize;
        let mut acc = 0u64;
        let mut step = n.next_power_of_two();
        while step > 0 {
            let next = pos + step;
            if next <= n && acc + key(&self.t[next]) <= target {
                pos = next;
                acc += key(&self.t[next]);
            }
            step >>= 1;
        }
        (pos, acc)
    }
}

/// The document's blocks.
#[derive(Debug, Clone, Default)]
pub struct BlockTree {
    leaves: Vec<Leaf>,
    index: Fenwick,
    total: Counts,
}

impl BlockTree {
    pub fn len(&self) -> usize {
        self.total.blocks as usize
    }
    pub fn is_empty(&self) -> bool {
        self.total.blocks == 0
    }
    pub fn counts(&self) -> Counts {
        self.total
    }

    /// (leaf, index in it) of block `i`.
    fn locate(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.len() {
            return None;
        }
        let (leaf, before) = self.index.search(i as u64, |c| c.blocks);
        Some((leaf, i - before as usize))
    }

    pub fn get(&self, i: usize) -> Option<&Block> {
        let (l, k) = self.locate(i)?;
        self.leaves[l].blocks.get(k)
    }

    /// Changes block `i` through `f`, keeping the counts right.
    pub fn update<R>(&mut self, i: usize, f: impl FnOnce(&mut Block) -> R) -> Option<R> {
        let (l, k) = self.locate(i)?;
        let before = count(&self.leaves[l].blocks[k]);
        let r = f(&mut self.leaves[l].blocks[k]);
        let after = count(&self.leaves[l].blocks[k]);
        let (db, dc) = (after.blocks as i64 - before.blocks as i64, after.chars as i64 - before.chars as i64);
        if db != 0 || dc != 0 {
            let leaf = &mut self.leaves[l];
            leaf.counts.blocks = (leaf.counts.blocks as i64 + db) as u64;
            leaf.counts.chars = (leaf.counts.chars as i64 + dc) as u64;
            self.index.add(l, db, dc);
            self.total.blocks = (self.total.blocks as i64 + db) as u64;
            self.total.chars = (self.total.chars as i64 + dc) as u64;
        }
        Some(r)
    }

    /// Inserts `b` before block `i` (`i == len` appends).
    pub fn insert(&mut self, i: usize, b: Block) {
        let i = i.min(self.len());
        if self.leaves.is_empty() {
            self.leaves.push(Leaf::default());
        }
        let (l, k) = if i == self.len() {
            let l = self.leaves.len() - 1;
            (l, self.leaves[l].blocks.len())
        } else {
            self.locate(i).expect("in range")
        };
        let c = count(&b);
        self.leaves[l].blocks.insert(k, b);
        self.leaves[l].counts.blocks += c.blocks;
        self.leaves[l].counts.chars += c.chars;
        self.total.blocks += c.blocks;
        self.total.chars += c.chars;
        if self.leaves[l].blocks.len() > LEAF {
            let rest = self.leaves[l].blocks.split_off(LEAF / 2);
            let mut nl = Leaf { blocks: rest, counts: Counts::default() };
            nl.recount();
            self.leaves[l].recount();
            self.leaves.insert(l + 1, nl);
            self.index = Fenwick::build(&self.leaves);
        } else {
            self.index.add(l, c.blocks as i64, c.chars as i64);
        }
    }

    /// Removes and returns block `i`.
    pub fn remove(&mut self, i: usize) -> Option<Block> {
        let (l, k) = self.locate(i)?;
        let b = self.leaves[l].blocks.remove(k);
        let c = count(&b);
        self.leaves[l].counts.blocks -= c.blocks;
        self.leaves[l].counts.chars -= c.chars;
        self.total.blocks -= c.blocks;
        self.total.chars -= c.chars;
        if self.leaves[l].blocks.is_empty() && self.leaves.len() > 1 {
            self.leaves.remove(l);
            self.index = Fenwick::build(&self.leaves);
        } else {
            self.index.add(l, -(c.blocks as i64), -(c.chars as i64));
        }
        Some(b)
    }

    /// The block holding document character `c` (counting each block's end as one
    /// character) and the character offset in it.
    pub fn block_at_char(&self, c: u64) -> Option<(usize, u64)> {
        if c >= self.total.chars {
            return None;
        }
        let (l, before) = self.index.search(c, |x| x.chars);
        let mut at = before;
        let first = self.index.prefix(l).blocks as usize;
        for (k, b) in self.leaves[l].blocks.iter().enumerate() {
            let n = count(b).chars;
            if c < at + n {
                return Some((first + k, c - at));
            }
            at += n;
        }
        None
    }

    /// The first document character of block `i`.
    pub fn char_of_block(&self, i: usize) -> u64 {
        let Some((l, k)) = self.locate(i) else { return self.total.chars };
        self.index.prefix(l).chars + self.leaves[l].blocks[..k].iter().map(|b| count(b).chars).sum::<u64>()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Block> + '_ {
        self.leaves.iter().flat_map(|l| l.blocks.iter())
    }

    /// Blocks from `i` on.
    pub fn iter_from(&self, i: usize) -> impl Iterator<Item = &Block> + '_ {
        let (l, k) = self.locate(i).unwrap_or((self.leaves.len(), 0));
        self.leaves.iter().skip(l).enumerate().flat_map(move |(j, leaf)| leaf.blocks.iter().skip(if j == 0 { k } else { 0 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Para;

    fn p(id: u64, t: &str) -> Block {
        Block::Para(Para::new(id, t, 0))
    }

    #[test]
    fn index_matches_a_list_through_inserts_edits_and_removes() {
        let mut t = BlockTree::default();
        let mut v: Vec<Block> = Vec::new();
        let mut x: u64 = 0x1234_5678;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        for id in 0..5000u64 {
            let i = rnd(v.len() as u64 + 1) as usize;
            let s = "é".repeat(rnd(9) as usize);
            t.insert(i, p(id, &s));
            v.insert(i, p(id, &s));
            if id % 7 == 0 && !v.is_empty() {
                let j = rnd(v.len() as u64) as usize;
                assert_eq!(t.remove(j), Some(v.remove(j)));
            }
            if id % 5 == 0 && !v.is_empty() {
                let j = rnd(v.len() as u64) as usize;
                t.update(j, |b| b.as_para_mut().unwrap().text.push_str("abc"));
                v[j].as_para_mut().unwrap().text.push_str("abc");
            }
        }
        assert_eq!(t.len(), v.len());
        assert_eq!(t.iter().cloned().collect::<Vec<_>>(), v);
        let chars: u64 = v.iter().map(|b| b.as_para().unwrap().text.chars().count() as u64 + 1).sum();
        assert_eq!(t.counts().chars, chars);
        // Character lookups agree with a walk.
        let mut at = 0u64;
        for (i, b) in v.iter().enumerate().step_by(37) {
            let _ = b;
            let start: u64 = v[..i].iter().map(|b| b.as_para().unwrap().text.chars().count() as u64 + 1).sum();
            assert_eq!(t.char_of_block(i), start);
            assert_eq!(t.block_at_char(start), Some((i, 0)));
            at = start;
        }
        let _ = at;
        assert_eq!(t.block_at_char(chars), None);
        assert_eq!(t.iter_from(10).next(), v.get(10));
    }
}
