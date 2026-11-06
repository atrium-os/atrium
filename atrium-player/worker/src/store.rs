//! The coded samples of one stream, in decode order, with a cursor at the
//! next one to decode. In a segment feed the samples stay until the parent
//! removes them (MSE's buffered ranges); in `Load` mode played ones go.
use std::collections::VecDeque;

use atrium_media_demux::Sample;

/// Samples closer than this count as one buffered range.
const GAP_NS: i64 = 100_000_000;
/// Duration of a sample whose container didn't say.
const DEFAULT_DUR_NS: i64 = 40_000_000;

#[derive(Default)]
pub struct Store {
    s: VecDeque<Sample>,
    cursor: usize,
    bytes: u64,
    /// A seek to a time not yet buffered (a segment feed): nothing is
    /// decoded until appends cover it, then the cursor goes there.
    pending: Option<i64>,
}

impl Store {
    pub fn push(&mut self, s: Sample) {
        self.bytes += s.data.len() as u64;
        match self.s.back() {
            Some(b) if s.dts_ns <= b.dts_ns => {
                // An overlapping append (MSE): replace a sample at the same
                // time, or insert in order.
                let i = self.s.partition_point(|x| x.dts_ns < s.dts_ns);
                if self.s.get(i).is_some_and(|x| x.dts_ns == s.dts_ns) {
                    self.bytes -= self.s[i].data.len() as u64;
                    self.s[i] = s;
                } else {
                    self.s.insert(i, s);
                    if i < self.cursor {
                        self.cursor += 1;
                    }
                }
            }
            _ => self.s.push_back(s),
        }
        if let Some(t) = self.pending {
            self.seek(t);
        }
    }

    pub fn next(&self) -> Option<&Sample> {
        if self.pending.is_some() {
            return None;
        }
        self.s.get(self.cursor)
    }


    pub fn advance(&mut self) {
        self.cursor = (self.cursor + 1).min(self.s.len());
    }

    pub fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// Samples not yet decoded.
    pub fn ahead(&self) -> usize {
        if self.pending.is_some() {
            return 0;
        }
        self.s.len() - self.cursor
    }

    /// Media time buffered ahead of the cursor.
    pub fn ahead_ns(&self) -> i64 {
        match (self.s.get(self.cursor), self.s.back()) {
            (Some(a), Some(b)) => b.pts_ns.saturating_sub(a.pts_ns),
            _ => 0,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn clear(&mut self) {
        self.s.clear();
        self.cursor = 0;
        self.bytes = 0;
        self.pending = None;
    }

    /// Drop what's been decoded (`Load` mode).
    pub fn evict_played(&mut self) {
        for _ in 0..self.cursor {
            if let Some(x) = self.s.pop_front() {
                self.bytes -= x.data.len() as u64;
            }
        }
        self.cursor = 0;
    }

    /// Move the cursor to the keyframe at or before `t_ns`, if that and
    /// `t_ns` itself are buffered. If not, hold decoding until appends
    /// cover it (or the caller clears the store and reads again).
    pub fn seek(&mut self, t_ns: i64) -> bool {
        let found = self.find(t_ns);
        self.pending = (!found).then_some(t_ns);
        found
    }

    fn find(&mut self, t_ns: i64) -> bool {
        let covered = self.ranges().iter().any(|&(a, b)| a <= t_ns.max(self.s.front().map_or(0, |x| x.pts_ns)) && t_ns < b);
        if !covered {
            return false;
        }
        let Some(k) = self.s.iter().enumerate().filter(|(_, x)| x.key && x.pts_ns <= t_ns).map(|(i, _)| i).next_back() else {
            return false;
        };
        self.cursor = k;
        true
    }

    /// Remove samples with presentation times in `[start, end)`.
    pub fn remove(&mut self, start: i64, end: i64) {
        let mut i = 0;
        while i < self.s.len() {
            let t = self.s[i].pts_ns;
            if t >= start && t < end {
                let x = self.s.remove(i).unwrap();
                self.bytes -= x.data.len() as u64;
                if i < self.cursor {
                    self.cursor -= 1;
                }
            } else {
                i += 1;
            }
        }
        self.cursor = self.cursor.min(self.s.len());
    }

    /// Buffered ranges in presentation time, merged across small gaps.
    pub fn ranges(&self) -> Vec<(i64, i64)> {
        let mut spans: Vec<(i64, i64)> = self
            .s
            .iter()
            .map(|x| (x.pts_ns, x.pts_ns.saturating_add(x.duration_ns.map_or(DEFAULT_DUR_NS, |d| d.min(10_000_000_000) as i64))))
            .collect();
        spans.sort_unstable();
        let mut out: Vec<(i64, i64)> = Vec::new();
        for (a, b) in spans {
            match out.last_mut() {
                Some(l) if a <= l.1 + GAP_NS => l.1 = l.1.max(b),
                _ => out.push((a, b)),
            }
        }
        out
    }
}

/// Where both streams are buffered (MSE reports the intersection).
pub fn intersect(a: &[(i64, i64)], b: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    for &(a0, a1) in a {
        for &(b0, b1) in b {
            let (s, e) = (a0.max(b0), a1.min(b1));
            if s < e {
                out.push((s, e));
            }
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pts_ms: i64, key: bool) -> Sample {
        Sample { track: 0, pts_ns: pts_ms * 1_000_000, dts_ns: pts_ms * 1_000_000, duration_ns: Some(40_000_000), key, data: vec![0; 10] }
    }

    #[test]
    fn ranges_seek_remove() {
        let mut st = Store::default();
        for (t, k) in [(0, true), (40, false), (80, false), (1000, true), (1040, false)] {
            st.push(s(t, k));
        }
        assert_eq!(st.ranges(), vec![(0, 120_000_000), (1_000_000_000, 1_080_000_000)]);
        assert!(st.seek(1_050_000_000));
        assert_eq!(st.next().unwrap().pts_ns, 1_000_000_000);
        assert!(!st.seek(500_000_000), "a gap isn't buffered");
        assert!(st.next().is_none(), "nothing to decode until it is");
        st.push(s(480, true));
        st.push(s(520, false));
        assert_eq!(st.next().unwrap().pts_ns, 480_000_000, "the append covering it moves the cursor there");
        assert!(st.seek(1_050_000_000));
        st.remove(0, 900_000_000);
        assert_eq!(st.ranges(), vec![(1_000_000_000, 1_080_000_000)]);
        assert_eq!(st.bytes(), 20);
        assert_eq!(st.next().unwrap().pts_ns, 1_000_000_000);
        // Overlapping append replaces.
        st.push(s(1000, true));
        assert_eq!(st.bytes(), 20);
        assert_eq!(intersect(&[(0, 10), (20, 30)], &[(5, 25)]), vec![(5, 10), (20, 25)]);
    }
}
