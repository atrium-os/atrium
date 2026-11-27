//! List numbering (officina.md §7.4: numbers derived from counts, not stored).
//!
//! An item's number is how many items of its list at its level came before it
//! since the last item at a shallower level. [`Numbering::build`] walks the
//! blocks once (a few microseconds per thousand paragraphs) and is rebuilt
//! when the document changes; labels never change layout (they hang in the
//! indent), so numbering doesn't take part in pagination.

use std::collections::HashMap;

use crate::model::{ListLevel, ListRef, NumFormat, TextDoc, MAX_LIST_LEVEL, MAX_LIST_START};

/// Each list item's ordinal (1-based) and label by block index.
#[derive(Debug, Clone, Default)]
pub struct Numbering {
    ordinals: HashMap<usize, (ListRef, u32, String)>,
    /// The largest list id in use.
    pub max_id: Option<u32>,
}

impl Numbering {
    pub fn build(doc: &TextDoc) -> Numbering {
        let mut n = Numbering::default();
        // Per list: each level's count so far (0: none yet at that level).
        let mut counters: HashMap<u32, [u32; MAX_LIST_LEVEL as usize + 1]> = HashMap::new();
        for (i, b) in doc.blocks.iter().enumerate() {
            let Some(p) = b.as_para() else { continue };
            if p.props.list == Some(None) {
                continue;
            }
            let l = match p.props.list {
                Some(Some(l)) => l,
                _ => match doc.styles.para_format(p.style, &p.props).0.list {
                    Some(l) => l,
                    None => continue,
                },
            };
            let c = counters.entry(l.id).or_default();
            let lv = l.level.min(MAX_LIST_LEVEL) as usize;
            let start = |k: usize| doc.styles.list_level(ListRef { level: k as u8, ..l }).start;
            c[lv] = match p.props.list_start {
                Some(s) => s.min(MAX_LIST_START),
                None if c[lv] == 0 => start(lv),
                None => c[lv].saturating_add(1),
            };
            for d in c.iter_mut().skip(lv + 1) {
                *d = 0;
            }
            // Levels above with no item yet count from their start.
            let nums: Vec<u32> = (0..=MAX_LIST_LEVEL as usize).map(|k| if k <= lv && c[k] == 0 { start(k) } else { c[k] }).collect();
            let label = level_label(&doc.styles.list_level(l), &nums, |k| doc.styles.list_level(ListRef { level: k as u8, ..l }).format);
            n.ordinals.insert(i, (l, c[lv], label));
            n.max_id = Some(n.max_id.map_or(l.id, |m| m.max(l.id)));
        }
        n
    }

    /// The label of block `i`, if it is a list item: "•", "3.", "b.", "iv.".
    pub fn label(&self, i: usize) -> Option<String> {
        self.ordinals.get(&i).map(|x| x.2.clone())
    }

    pub fn item(&self, i: usize) -> Option<(ListRef, u32)> {
        self.ordinals.get(&i).map(|x| (x.0, x.1))
    }
}

/// A built-in list's label for item `k` (no definition of its own).
pub fn label(l: ListRef, k: u32) -> String {
    let lv = ListLevel::builtin(l.kind, l.level);
    let mut nums = vec![1; MAX_LIST_LEVEL as usize + 1];
    nums[l.level.min(MAX_LIST_LEVEL) as usize] = k;
    // Only the item's own level shows in a built-in label.
    level_label(&lv, &nums, |k| ListLevel::builtin(l.kind, k as u8).format)
}

/// `lv`'s label with `%1`…`%9` read from `nums`, each in its level's format.
pub fn level_label(lv: &ListLevel, nums: &[u32], format_of: impl Fn(usize) -> NumFormat) -> String {
    if lv.format == NumFormat::Bullet {
        return lv.text.to_string();
    }
    let mut out = String::new();
    let mut it = lv.text.chars().peekable();
    while let Some(ch) = it.next() {
        match (ch, it.peek().and_then(|d| d.to_digit(10))) {
            ('%', Some(d @ 1..=9)) => {
                it.next();
                let k = d as usize - 1;
                out.push_str(&number(nums.get(k).copied().unwrap_or(1), format_of(k)));
            }
            _ => out.push(ch),
        }
    }
    out
}

/// `k` in `f`.
pub fn number(k: u32, f: NumFormat) -> String {
    match f {
        NumFormat::Decimal => k.to_string(),
        NumFormat::DecimalZero => format!("{k:02}"),
        NumFormat::LowerLetter => alpha(k),
        NumFormat::UpperLetter => alpha(k).to_uppercase(),
        NumFormat::LowerRoman => roman(k),
        NumFormat::UpperRoman => roman(k).to_uppercase(),
        NumFormat::Bullet | NumFormat::None => String::new(),
    }
}

/// a, b, … z, aa, ab, …
fn alpha(mut k: u32) -> String {
    let mut s = Vec::new();
    while k > 0 {
        k -= 1;
        s.push(b'a' + (k % 26) as u8);
        k /= 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

pub fn roman(mut k: u32) -> String {
    if k == 0 || k > 3999 {
        return k.to_string();
    }
    let t = [(1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")];
    let mut s = String::new();
    for (v, r) in t {
        while k >= v {
            s.push_str(r);
            k -= v;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ListDef, ListKind, ParaProps};

    #[test]
    fn numbers_by_level_and_list() {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let item = |id, kind, level| ParaProps { list: Some(Some(ListRef { id, kind, level })), ..Default::default() };
        let props = [
            Some(item(1, ListKind::Numbered, 0)),
            Some(item(1, ListKind::Numbered, 1)),
            Some(item(1, ListKind::Numbered, 1)),
            Some(item(1, ListKind::Numbered, 2)),
            None,
            Some(item(1, ListKind::Numbered, 0)),
            Some(item(1, ListKind::Numbered, 1)),
            Some(item(2, ListKind::Bullet, 1)),
            Some(item(2, ListKind::Numbered, 0)),
        ];
        for p in &props {
            let i = d.push_para("x", 0, vec![]);
            if let Some(p) = p {
                d.blocks.update(i, |b| b.as_para_mut().unwrap().props = p.clone());
            }
        }
        let n = Numbering::build(&d);
        let labels: Vec<Option<String>> = (0..props.len()).map(|i| n.label(i)).collect();
        let want = ["1.", "a.", "b.", "i.", "", "2.", "a.", "◦", "1."];
        for (l, w) in labels.iter().zip(want) {
            assert_eq!(l.as_deref().unwrap_or(""), w);
        }
        assert_eq!(n.max_id, Some(2));
        assert_eq!(alpha(27), "aa");
        assert_eq!(roman(1994), "mcmxciv");
        // List items hang their labels at their level's indent.
        let pf = d.styles.para_format(0, &d.para(1).unwrap().props).0;
        assert_eq!((pf.indent_start, pf.first_line), (1440, -360));
    }

    #[test]
    fn a_lists_own_levels_and_a_restart_number_its_items() {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let mut def = ListDef::builtin(ListKind::Numbered);
        def.levels[0] = ListLevel { format: NumFormat::UpperRoman, text: "Part %1:".into(), start: 3, indent: 900, hang: 400 };
        def.levels[1] = ListLevel { format: NumFormat::Decimal, text: "%1.%2".into(), start: 1, indent: 1800, hang: 500 };
        d.styles.lists.insert(4, def);
        let item = |level, start| ParaProps { list: Some(Some(ListRef { id: 4, kind: ListKind::Numbered, level })), list_start: start, ..Default::default() };
        for p in [item(0, None), item(1, None), item(1, None), item(0, None), item(1, None), item(0, Some(1))] {
            let i = d.push_para("x", 0, vec![]);
            d.blocks.update(i, |b| b.as_para_mut().unwrap().props = p.clone());
        }
        let n = Numbering::build(&d);
        let labels: Vec<String> = (0..6).map(|i| n.label(i).unwrap()).collect();
        // Each %n takes its own level's format.
        assert_eq!(labels, ["Part III:", "III.1", "III.2", "Part IV:", "IV.1", "Part I:"]);
        let pf = d.styles.para_format(0, &d.para(1).unwrap().props).0;
        assert_eq!((pf.indent_start, pf.first_line), (1800, -500));
    }
}
