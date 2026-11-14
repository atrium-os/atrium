//! List numbering (officina.md §7.4: numbers derived from counts, not stored).
//!
//! An item's number is how many items of its list at its level came before it
//! since the last item at a shallower level. [`Numbering::build`] walks the
//! blocks once (a few microseconds per thousand paragraphs) and is rebuilt
//! when the document changes; labels never change layout (they hang in the
//! indent), so numbering doesn't take part in pagination.

use std::collections::HashMap;

use crate::model::{ListKind, ListRef, TextDoc, MAX_LIST_LEVEL};

/// Each list item's ordinal (1-based) by block index.
#[derive(Debug, Clone, Default)]
pub struct Numbering {
    ordinals: HashMap<usize, (ListRef, u32)>,
    /// The largest list id in use.
    pub max_id: Option<u32>,
}

impl Numbering {
    pub fn build(doc: &TextDoc) -> Numbering {
        let mut n = Numbering::default();
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
            c[lv] += 1;
            for d in c.iter_mut().skip(lv + 1) {
                *d = 0;
            }
            n.ordinals.insert(i, (l, c[lv]));
            n.max_id = Some(n.max_id.map_or(l.id, |m| m.max(l.id)));
        }
        n
    }

    /// The label of block `i`, if it is a list item: "•", "3.", "b.", "iv.".
    pub fn label(&self, i: usize) -> Option<String> {
        let &(l, k) = self.ordinals.get(&i)?;
        Some(label(l, k))
    }

    pub fn item(&self, i: usize) -> Option<(ListRef, u32)> {
        self.ordinals.get(&i).copied()
    }
}

pub fn label(l: ListRef, k: u32) -> String {
    match l.kind {
        ListKind::Bullet => ["•", "◦", "▪"][l.level as usize % 3].to_string(),
        ListKind::Numbered => match l.level % 3 {
            0 => format!("{k}."),
            1 => format!("{}.", alpha(k)),
            _ => format!("{}.", roman(k)),
        },
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
    use crate::model::ParaProps;

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
}
