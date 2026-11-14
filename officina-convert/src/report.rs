//! The fidelity report every import ends with (docs/spec/officina.md §5.2): what was
//! converted approximately, what was kept inert, what was dropped. No silent loss.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Shown, but not exactly as the file has it.
    Approximate,
    /// Present, never run (macros, external links).
    Inert,
    /// Not carried over.
    Dropped,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Approximate => "approximate",
            Kind::Inert => "inert",
            Kind::Dropped => "dropped",
        }
    }
    pub fn from_word(w: &str) -> Option<Self> {
        Some(match w {
            "approximate" => Kind::Approximate,
            "inert" => Kind::Inert,
            "dropped" => Kind::Dropped,
            _ => return None,
        })
    }
}

/// Notes, counted: the same thing found a million times is one line with a count.
#[derive(Debug, Default, Clone)]
pub struct Report {
    notes: BTreeMap<(Kind, String), u64>,
    /// A few examples per note (a cell address, a formula).
    examples: BTreeMap<(Kind, String), Vec<String>>,
}

const EXAMPLES: usize = 3;

impl Report {
    pub fn note(&mut self, kind: Kind, what: &str) {
        *self.notes.entry((kind, what.to_string())).or_insert(0) += 1;
    }

    pub fn note_n(&mut self, kind: Kind, what: &str, n: u64) {
        *self.notes.entry((kind, what.to_string())).or_insert(0) += n;
    }

    pub fn note_at(&mut self, kind: Kind, what: &str, example: impl FnOnce() -> String) {
        let k = (kind, what.to_string());
        *self.notes.entry(k.clone()).or_insert(0) += 1;
        let ex = self.examples.entry(k).or_default();
        if ex.len() < EXAMPLES {
            ex.push(example());
        }
    }

    /// Adds `other`'s notes to these.
    pub fn merge(&mut self, other: Report) {
        for (k, n) in other.notes {
            *self.notes.entry(k).or_insert(0) += n;
        }
        for (k, ex) in other.examples {
            let mine = self.examples.entry(k).or_default();
            for e in ex {
                if mine.len() < EXAMPLES {
                    mine.push(e);
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// One line per note: `kind<TAB>count<TAB>what<TAB>examples`.
    pub fn lines(&self) -> Vec<String> {
        self.notes
            .iter()
            .map(|(k, n)| {
                let ex = self.examples.get(k).map(|e| e.join(", ")).unwrap_or_default();
                format!("{}\t{}\t{}\t{}", k.0.word(), n, k.1.replace(['\t', '\n'], " "), ex.replace(['\t', '\n'], " "))
            })
            .collect()
    }

    pub fn parse_line(line: &str) -> Option<(Kind, u64, String, String)> {
        let mut it = line.splitn(4, '\t');
        let kind = Kind::from_word(it.next()?)?;
        let n = it.next()?.parse().ok()?;
        let what = it.next()?.to_string();
        let ex = it.next().unwrap_or("").to_string();
        Some((kind, n, what, ex))
    }

    /// For people: "approximate: 1,203 × formula kept as its value (unknown function) — e.g. B2, C9".
    pub fn human(&self) -> Vec<String> {
        self.notes
            .iter()
            .map(|(k, n)| {
                let ex = self.examples.get(k).filter(|e| !e.is_empty()).map(|e| format!(" (e.g. {})", e.join(", "))).unwrap_or_default();
                if *n == 1 {
                    format!("{}: {}{ex}", k.0.word(), k.1)
                } else {
                    format!("{}: {} × {}{ex}", k.0.word(), n, k.1)
                }
            })
            .collect()
    }
}
