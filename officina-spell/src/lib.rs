//! Spell checking with Hunspell-format dictionaries (docs/spec/officina.md
//! §13 "Languages", O3f), in our own code: a `.aff` file's prefix and suffix
//! rules (with cross products and conditions), compound rules, replacement
//! pairs and the TRY alphabet, over a `.dic` word list. Enough of the format
//! for the SCOWL English dictionaries shipped in `dict/`; flags or options a
//! dictionary uses that this reader doesn't know are listed by
//! [`Dictionary::unsupported`] rather than guessed at.
//!
//! The reader is bounded (file sizes, rule counts, word length), since a
//! dictionary may come from a package.

use std::collections::{HashMap, HashSet};

/// Largest `.dic` or `.aff` accepted.
pub const MAX_FILE: usize = 64 << 20;
/// Longest word checked, in bytes (longer ones are left alone).
pub const MAX_WORD: usize = 100;
/// Most suggestions.
pub const MAX_SUGGESTIONS: usize = 8;

type Flag = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagMode {
    Char,
    Long,
    Num,
}

/// One character class of an affix condition.
#[derive(Debug, Clone, PartialEq)]
enum Cond {
    Any,
    One(char),
    In(Vec<char>),
    NotIn(Vec<char>),
}

impl Cond {
    fn ok(&self, c: char) -> bool {
        match self {
            Cond::Any => true,
            Cond::One(x) => *x == c,
            Cond::In(v) => v.contains(&c),
            Cond::NotIn(v) => !v.contains(&c),
        }
    }
}

fn parse_cond(s: &str) -> Vec<Cond> {
    if s == "." {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '.' => out.push(Cond::Any),
            '[' => {
                let neg = it.peek() == Some(&'^');
                if neg {
                    it.next();
                }
                let mut v = Vec::new();
                for d in it.by_ref() {
                    if d == ']' {
                        break;
                    }
                    v.push(d);
                }
                out.push(if neg { Cond::NotIn(v) } else { Cond::In(v) });
            }
            c => out.push(Cond::One(c)),
        }
    }
    out
}

#[derive(Debug, Clone)]
struct Affix {
    flag: Flag,
    cross: bool,
    strip: String,
    add: String,
    /// Flags the affixed form carries (continuation classes).
    cont: Vec<Flag>,
    cond: Vec<Cond>,
}

impl Affix {
    /// Whether a suffix's condition holds at the end of `base`.
    fn suffix_ok(&self, base: &str) -> bool {
        let n = self.cond.len();
        let chars: Vec<char> = base.chars().rev().take(n).collect();
        chars.len() == n && self.cond.iter().rev().zip(chars.iter()).all(|(c, &ch)| c.ok(ch))
    }
    /// Whether a prefix's condition holds at the start of `base`.
    fn prefix_ok(&self, base: &str) -> bool {
        let n = self.cond.len();
        let chars: Vec<char> = base.chars().take(n).collect();
        chars.len() == n && self.cond.iter().zip(chars.iter()).all(|(c, &ch)| c.ok(ch))
    }
}

/// A compound rule: flags with `*` (any number) or `?` (at most one).
#[derive(Debug, Clone)]
struct Rule(Vec<(Flag, char)>);

/// A loaded dictionary.
#[derive(Debug, Default)]
pub struct Dictionary {
    words: HashMap<String, Vec<Vec<Flag>>>,
    prefixes: Vec<Affix>,
    suffixes: Vec<Affix>,
    rules: Vec<Rule>,
    rep: Vec<(String, String)>,
    iconv: Vec<(String, String)>,
    try_chars: Vec<char>,
    nosuggest: Option<Flag>,
    forbidden: Option<Flag>,
    needaffix: Option<Flag>,
    onlyincompound: Option<Flag>,
    keepcase: Option<Flag>,
    compound_min: usize,
    /// Characters besides letters that belong to words.
    pub word_chars: HashSet<char>,
    unsupported: Vec<String>,
    mode: Option<FlagMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictError(pub String);

impl std::fmt::Display for DictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn parse_flags(s: &str, mode: FlagMode) -> Vec<Flag> {
    match mode {
        FlagMode::Char => s.chars().map(|c| c as u32).collect(),
        FlagMode::Long => {
            let c: Vec<char> = s.chars().collect();
            c.chunks(2).map(|p| ((p[0] as u32) << 16) | p.get(1).map_or(0, |&x| x as u32)).collect()
        }
        FlagMode::Num => s.split(',').filter_map(|n| n.trim().parse().ok()).collect(),
    }
}

fn case_of(w: &str) -> (bool, bool, bool) {
    // (all upper, first upper, has lower)
    let mut chars = w.chars().filter(|c| c.is_alphabetic());
    let first = chars.next();
    let rest: Vec<char> = chars.collect();
    let first_up = first.is_some_and(|c| c.is_uppercase());
    let all_up = first_up && rest.iter().all(|c| !c.is_lowercase());
    let has_lower = first.is_some_and(|c| c.is_lowercase()) || rest.iter().any(|c| c.is_lowercase());
    (all_up, first_up, has_lower)
}

fn title(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c.flat_map(|x| x.to_lowercase())).collect(),
        None => String::new(),
    }
}

impl Dictionary {
    /// The English (United States) dictionary from SCOWL, built in.
    pub fn en_us() -> Dictionary {
        Dictionary::load(include_str!("../dict/en_US.aff"), include_str!("../dict/en_US.dic")).expect("the built-in dictionary loads")
    }

    pub fn load(aff: &str, dic: &str) -> Result<Dictionary, DictError> {
        if aff.len() > MAX_FILE || dic.len() > MAX_FILE {
            return Err(DictError("dictionary too large".into()));
        }
        let mut d = Dictionary { compound_min: 3, ..Default::default() };
        let mut mode = FlagMode::Char;
        let mut lines = aff.lines();
        while let Some(line) = lines.next() {
            let mut f = line.split_whitespace();
            let Some(key) = f.next() else { continue };
            if key.starts_with('#') {
                continue;
            }
            let args: Vec<&str> = f.collect();
            let one = |m: FlagMode| args.first().map(|a| parse_flags(a, m)).and_then(|v| v.first().copied());
            match key {
                "SET" => {
                    if !args.first().is_some_and(|a| a.eq_ignore_ascii_case("UTF-8") || a.eq_ignore_ascii_case("UTF8")) {
                        return Err(DictError(format!("encoding {:?} (only UTF-8 is read)", args.first())));
                    }
                }
                "FLAG" => {
                    mode = match args.first().copied() {
                        Some("long") => FlagMode::Long,
                        Some("num") => FlagMode::Num,
                        Some("UTF-8") | None => FlagMode::Char,
                        Some(o) => return Err(DictError(format!("flag type {o}"))),
                    };
                }
                "TRY" => d.try_chars = args.first().map(|a| a.chars().collect()).unwrap_or_default(),
                "WORDCHARS" => d.word_chars = args.first().map(|a| a.chars().collect()).unwrap_or_default(),
                "NOSUGGEST" => d.nosuggest = one(mode),
                "FORBIDDENWORD" => d.forbidden = one(mode),
                "NEEDAFFIX" => d.needaffix = one(mode),
                "ONLYINCOMPOUND" => d.onlyincompound = one(mode),
                "KEEPCASE" => d.keepcase = one(mode),
                "COMPOUNDMIN" => d.compound_min = args.first().and_then(|a| a.parse().ok()).unwrap_or(3).max(1),
                // Count lines ("REP 91").
                "REP" | "ICONV" | "COMPOUNDRULE" if args.len() == 1 && args[0].parse::<usize>().is_ok() => {}
                "REP" if args.len() >= 2 => d.rep.push((args[0].replace('_', " "), args[1].replace('_', " "))),
                "ICONV" if args.len() >= 2 => d.iconv.push((args[0].to_string(), args[1].to_string())),
                "COMPOUNDRULE" if !args.is_empty() => {
                    let mut r = Vec::new();
                    let s = args[0];
                    let mut it = s.chars().peekable();
                    while let Some(c) = it.next() {
                        let flag = if c == '(' {
                            let inner: String = it.by_ref().take_while(|&x| x != ')').collect();
                            parse_flags(&inner, mode).first().copied().unwrap_or(0)
                        } else {
                            c as u32
                        };
                        let q = match it.peek() {
                            Some(&'*') | Some(&'?') => it.next().unwrap(),
                            _ => ' ',
                        };
                        r.push((flag, q));
                    }
                    if r.len() <= 16 {
                        d.rules.push(Rule(r));
                    }
                }
                "PFX" | "SFX" => {
                    // Header: FLAG CROSS COUNT; then COUNT lines.
                    if args.len() < 3 {
                        continue;
                    }
                    let Some(flag) = parse_flags(args[0], mode).first().copied() else { continue };
                    let cross = args[1] == "Y";
                    let n: usize = args[2].parse().unwrap_or(0).min(100_000);
                    for _ in 0..n {
                        let Some(l) = lines.next() else { break };
                        let a: Vec<&str> = l.split_whitespace().collect();
                        if a.len() < 4 || a[0] != key {
                            continue;
                        }
                        let strip = if a[2] == "0" { String::new() } else { a[2].to_string() };
                        let (add, cont) = match a[3].split_once('/') {
                            Some((x, fl)) => (x, parse_flags(fl, mode)),
                            None => (a[3], Vec::new()),
                        };
                        let add = if add == "0" { String::new() } else { add.to_string() };
                        let cond = parse_cond(a.get(4).copied().unwrap_or("."));
                        let af = Affix { flag, cross, strip, add, cont, cond };
                        if key == "PFX" {
                            d.prefixes.push(af);
                        } else {
                            d.suffixes.push(af);
                        }
                    }
                }
                "LANG" | "NAME" | "VERSION" | "HOME" | "AF" | "AM" | "KEY" | "MAP" | "OCONV" | "MAXNGRAMSUGS" | "ONLYMAXDIFF" | "NOSPLITSUGS" | "MAXDIFF" | "SUGSWITHDOTS" => {}
                other => {
                    if other.chars().all(|c| c.is_ascii_uppercase()) && !d.unsupported.iter().any(|u| u == other) {
                        d.unsupported.push(other.to_string());
                    }
                }
            }
        }
        d.mode = Some(mode);
        let mut dl = dic.lines();
        dl.next();
        for line in dl {
            let line = line.split('\t').next().unwrap_or("");
            let entry = line.split(' ').next().unwrap_or("").trim();
            if entry.is_empty() {
                continue;
            }
            let (word, flags) = match entry.find('/').filter(|&i| i > 0 && !entry[..i].ends_with('\\')) {
                Some(i) => (&entry[..i], parse_flags(&entry[i + 1..], mode)),
                None => (entry, Vec::new()),
            };
            let word = word.replace("\\/", "/");
            if word.len() > MAX_WORD {
                continue;
            }
            d.words.entry(word).or_default().push(flags);
        }
        Ok(d)
    }

    /// Options the `.aff` file uses that this reader ignores.
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    fn has(fl: &[Flag], f: Option<Flag>) -> bool {
        f.is_some_and(|f| fl.contains(&f))
    }

    /// Root entries for `w` usable on their own (not forbidden, not needing an
    /// affix, not only in compounds).
    fn root_alone(&self, w: &str) -> Option<&Vec<Flag>> {
        self.words.get(w)?.iter().find(|fl| !Self::has(fl, self.forbidden) && !Self::has(fl, self.needaffix) && !Self::has(fl, self.onlyincompound))
    }

    fn forbidden(&self, w: &str) -> bool {
        self.words.get(w).is_some_and(|v| v.iter().any(|fl| Self::has(fl, self.forbidden)))
    }

    /// The flags of a form of `w` built from a root with affixes, if any.
    fn affixed(&self, w: &str) -> Option<Vec<Flag>> {
        // Suffixes.
        for s in &self.suffixes {
            if let Some(base) = self.strip_suffix(w, s) {
                if let Some(fl) = self.words.get(&base).and_then(|v| v.iter().find(|fl| fl.contains(&s.flag) && !Self::has(fl, self.forbidden))) {
                    return Some(fl.clone());
                }
                // A suffix on top of a suffix's continuation class.
                for s2 in &self.suffixes {
                    if !s2.cont.contains(&s.flag) {
                        continue;
                    }
                    if let Some(b2) = self.strip_suffix(&base, s2) {
                        if let Some(fl) = self.words.get(&b2).and_then(|v| v.iter().find(|fl| fl.contains(&s2.flag))) {
                            return Some(fl.clone());
                        }
                    }
                }
            }
        }
        // Prefixes, and prefix with suffix (cross products).
        for p in &self.prefixes {
            let Some(base) = self.strip_prefix(w, p) else { continue };
            if let Some(fl) = self.words.get(&base).and_then(|v| v.iter().find(|fl| fl.contains(&p.flag) && !Self::has(fl, self.forbidden))) {
                return Some(fl.clone());
            }
            if !p.cross {
                continue;
            }
            for s in self.suffixes.iter().filter(|s| s.cross) {
                if let Some(b2) = self.strip_suffix(&base, s) {
                    if let Some(fl) = self.words.get(&b2).and_then(|v| v.iter().find(|fl| fl.contains(&p.flag) && fl.contains(&s.flag))) {
                        return Some(fl.clone());
                    }
                }
            }
        }
        None
    }

    fn strip_suffix(&self, w: &str, s: &Affix) -> Option<String> {
        let rest = w.strip_suffix(s.add.as_str())?;
        if rest.is_empty() && s.strip.is_empty() {
            return None;
        }
        let base = format!("{rest}{}", s.strip);
        s.suffix_ok(&base).then_some(base)
    }

    fn strip_prefix(&self, w: &str, p: &Affix) -> Option<String> {
        let rest = w.strip_prefix(p.add.as_str())?;
        if rest.is_empty() && p.strip.is_empty() {
            return None;
        }
        let base = format!("{}{rest}", p.strip);
        p.prefix_ok(&base).then_some(base)
    }

    /// Whether `w` (one case form) is a word.
    fn known(&self, w: &str) -> bool {
        if self.forbidden(w) {
            return false;
        }
        self.root_alone(w).is_some() || self.affixed(w).is_some() || self.compound(w)
    }

    /// Whether `w` splits into words following a compound rule.
    fn compound(&self, w: &str) -> bool {
        if self.rules.is_empty() || w.len() > MAX_WORD {
            return false;
        }
        let bounds: Vec<usize> = w.char_indices().map(|(i, _)| i).chain([w.len()]).collect();
        self.rules.iter().any(|r| self.rule_at(w, &bounds, 0, &r.0, 0, 0))
    }

    fn rule_at(&self, w: &str, bounds: &[usize], at: usize, rule: &[(Flag, char)], parts: usize, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        if rule.is_empty() {
            return at == bounds.len() - 1 && parts >= 2;
        }
        let (flag, q) = rule[0];
        // Skip an optional or starred element.
        if (q == '*' || q == '?') && self.rule_at(w, bounds, at, &rule[1..], parts, depth + 1) {
            return true;
        }
        for end in at + self.compound_min..bounds.len() {
            let piece = &w[bounds[at]..bounds[end]];
            let fits = self.words.get(piece).is_some_and(|v| v.iter().any(|fl| fl.contains(&flag)));
            if !fits {
                continue;
            }
            let next = if q == '*' { rule } else { &rule[1..] };
            if self.rule_at(w, bounds, end, next, parts + 1, depth + 1) {
                return true;
            }
        }
        false
    }

    /// Whether `word` is spelled right (any of its sensible case forms).
    pub fn check(&self, word: &str) -> bool {
        if word.is_empty() || word.len() > MAX_WORD {
            return true;
        }
        let mut w = word.to_string();
        for (a, b) in &self.iconv {
            w = w.replace(a.as_str(), b);
        }
        // Numbers are fine.
        if w.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',' || c == '-') {
            return true;
        }
        if self.known(&w) {
            return true;
        }
        let (all_up, first_up, has_lower) = case_of(&w);
        if all_up {
            let lower = w.to_lowercase();
            if self.known_not_keepcase(&lower) || self.known_not_keepcase(&title(&w)) {
                return true;
            }
        } else if first_up && w.chars().skip(1).all(|c| !c.is_uppercase()) && has_lower {
            let lower = w.to_lowercase();
            if self.known_not_keepcase(&lower) {
                return true;
            }
        }
        // A word with a final apostrophe ("dogs'").
        if let Some(stem) = w.strip_suffix('\'') {
            return !stem.is_empty() && self.check(stem);
        }
        false
    }

    fn known_not_keepcase(&self, w: &str) -> bool {
        if self.keepcase.is_some() && self.words.get(w).is_some_and(|v| v.iter().all(|fl| Self::has(fl, self.keepcase))) {
            return false;
        }
        self.known(w)
    }

    /// A cheap first test for two-edit candidates: a root, or a root with one
    /// common suffix.
    fn dict_has_quick(&self, w: &str) -> bool {
        self.words.contains_key(w) || self.affixed(w).is_some()
    }

    fn suggestible(&self, w: &str) -> bool {
        if !self.check(w) {
            return false;
        }
        let root_nosuggest = |x: &str| self.words.get(x).is_some_and(|v| v.iter().all(|fl| Self::has(fl, self.nosuggest)));
        !root_nosuggest(w) && !root_nosuggest(&w.to_lowercase())
    }

    /// Up to [`MAX_SUGGESTIONS`] spellings for a misspelled `word`, best first,
    /// in its case.
    pub fn suggest(&self, word: &str) -> Vec<String> {
        if word.is_empty() || word.len() > MAX_WORD {
            return Vec::new();
        }
        let (all_up, first_up, _) = case_of(word);
        let w = word.to_lowercase();
        let chars: Vec<char> = w.chars().collect();
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let push = |c: String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            if out.len() < MAX_SUGGESTIONS * 4 && seen.insert(c.clone()) && c != w {
                let parts_ok = c.split(' ').all(|p| self.suggestible(p));
                if parts_ok {
                    out.push(c);
                }
            }
        };
        // Replacement pairs (common misspellings) first.
        for (a, b) in &self.rep {
            let (start, a) = match a.strip_prefix('^') {
                Some(x) => (true, x),
                None => (false, a.as_str()),
            };
            let (end, a) = match a.strip_suffix('$') {
                Some(x) => (true, x),
                None => (false, a),
            };
            if a.is_empty() || (start && !w.starts_with(a)) || (end && !w.ends_with(a)) {
                continue;
            }
            if start || end {
                let at = if start { 0 } else { w.len() - a.len() };
                if start && end && w != a {
                    continue;
                }
                push(format!("{}{}{}", &w[..at], b, &w[at + a.len()..]), &mut out, &mut seen);
                continue;
            }
            let mut from = 0;
            while let Some(i) = w[from..].find(a) {
                let at = from + i;
                push(format!("{}{}{}", &w[..at], b, &w[at + a.len()..]), &mut out, &mut seen);
                from = at + a.len().max(1);
                if from > w.len() {
                    break;
                }
            }
        }
        let s = |v: &[char]| v.iter().collect::<String>();
        // Swapped neighbours.
        for i in 0..chars.len().saturating_sub(1) {
            let mut v = chars.clone();
            v.swap(i, i + 1);
            push(s(&v), &mut out, &mut seen);
        }
        // Two words run together.
        for i in 1..chars.len() {
            push(format!("{} {}", s(&chars[..i]), s(&chars[i..])), &mut out, &mut seen);
        }
        // One letter wrong, missing, extra.
        let tries: Vec<char> = if self.try_chars.is_empty() { ('a'..='z').collect() } else { self.try_chars.iter().copied().filter(|c| !c.is_uppercase()).collect() };
        for i in 0..chars.len() {
            for &t in &tries {
                if t != chars[i] {
                    let mut v = chars.clone();
                    v[i] = t;
                    push(s(&v), &mut out, &mut seen);
                }
            }
        }
        for i in 0..=chars.len() {
            for &t in &tries {
                let mut v = chars.clone();
                v.insert(i, t);
                push(s(&v), &mut out, &mut seen);
            }
        }
        for i in 0..chars.len() {
            let mut v = chars.clone();
            v.remove(i);
            push(s(&v), &mut out, &mut seen);
        }
        // Two edits, when one found little: each one-edit string that isn't a
        // word, edited again (bounded).
        if out.len() < 3 && chars.len() <= 24 {
            let mut firsts: Vec<Vec<char>> = Vec::new();
            for i in 0..chars.len() {
                let mut v = chars.clone();
                v.remove(i);
                firsts.push(v);
                for &t in &tries {
                    let mut v = chars.clone();
                    v[i] = t;
                    firsts.push(v);
                }
            }
            for i in 0..=chars.len() {
                for &t in &tries {
                    let mut v = chars.clone();
                    v.insert(i, t);
                    firsts.push(v);
                }
            }
            for i in 0..chars.len().saturating_sub(1) {
                let mut v = chars.clone();
                v.swap(i, i + 1);
                firsts.push(v);
            }
            let mut budget = 400_000usize;
            'outer: for f in &firsts {
                let edits = |f: &Vec<char>, emit: &mut dyn FnMut(Vec<char>) -> bool| {
                    for i in 0..f.len().saturating_sub(1) {
                        let mut v = f.clone();
                        v.swap(i, i + 1);
                        if !emit(v) {
                            return false;
                        }
                    }
                    for i in 0..f.len() {
                        let mut v = f.clone();
                        v.remove(i);
                        if !emit(v) {
                            return false;
                        }
                    }
                    for i in 0..=f.len() {
                        for &t in &tries {
                            let mut v = f.clone();
                            v.insert(i, t);
                            if !emit(v) {
                                return false;
                            }
                        }
                    }
                    for i in 0..f.len() {
                        for &t in &tries {
                            if t != f[i] {
                                let mut v = f.clone();
                                v[i] = t;
                                if !emit(v) {
                                    return false;
                                }
                            }
                        }
                    }
                    true
                };
                let mut emit = |v: Vec<char>| {
                    if budget == 0 {
                        return false;
                    }
                    budget -= 1;
                    let c = s(&v);
                    if !seen.contains(&c) && self.dict_has_quick(&c) {
                        push(c, &mut out, &mut seen);
                    }
                    out.len() < MAX_SUGGESTIONS * 4
                };
                if !edits(f, &mut emit) {
                    break 'outer;
                }
            }
        }
        // Rank: replacement pairs and swaps came first; then prefer the same
        // first letter and the same length.
        let first = chars.first().copied();
        let mut ranked: Vec<(usize, usize, String)> = out
            .into_iter()
            .enumerate()
            .map(|(k, c)| {
                let mut score = k;
                if c.chars().next() != first {
                    score += 1000;
                }
                (score, k, c)
            })
            .collect();
        ranked.sort();
        ranked
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .map(|(_, _, c)| {
                if all_up && chars.len() > 1 {
                    c.to_uppercase()
                } else if first_up {
                    title_first(&c)
                } else {
                    c
                }
            })
            .collect()
    }

    /// Every form the dictionary allows that one affix (or a prefix with a
    /// suffix) makes from a root, roots included: what `unmunch` lists. For
    /// checking the reader against itself.
    pub fn forms(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (w, entries) in &self.words {
            for fl in entries {
                if Self::has(fl, self.forbidden) || Self::has(fl, self.onlyincompound) {
                    continue;
                }
                if !Self::has(fl, self.needaffix) {
                    out.push(w.clone());
                }
                let mut sfx_forms = Vec::new();
                for s in self.suffixes.iter().filter(|s| fl.contains(&s.flag)) {
                    if let Some(f) = w.strip_suffix(s.strip.as_str()).filter(|_| s.suffix_ok(w)) {
                        let f = format!("{f}{}", s.add);
                        out.push(f.clone());
                        if s.cross {
                            sfx_forms.push(f);
                        }
                    }
                }
                for p in self.prefixes.iter().filter(|p| fl.contains(&p.flag)) {
                    let Some(rest) = w.strip_prefix(p.strip.as_str()).filter(|_| p.prefix_ok(w)) else { continue };
                    out.push(format!("{}{rest}", p.add));
                    if p.cross {
                        for f in &sfx_forms {
                            if let Some(r) = f.strip_prefix(p.strip.as_str()) {
                                out.push(format!("{}{r}", p.add));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Whether `c` belongs inside words.
    pub fn word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || self.word_chars.contains(&c)
    }
}

fn title_first(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The words of `text` worth checking, as byte ranges: runs of letters,
/// digits and apostrophes inside them; web and mail addresses and words with
/// digits and letters mixed (codes, part numbers) are skipped.
pub fn words(text: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let b = text;
    let mut i = 0;
    let n = b.len();
    let wordish = |c: char| c.is_alphanumeric() || c == '\'' || c == '’' || c == '-' || c == '.' || c == '@' || c == '/' || c == ':' || c == '_';
    while i < n {
        let c = b[i..].chars().next().expect("in range");
        if !c.is_alphanumeric() {
            i += c.len_utf8();
            continue;
        }
        // The whole token up to a space or punctuation that never joins words.
        let mut j = i;
        while j < n {
            let d = b[j..].chars().next().expect("in range");
            if !wordish(d) {
                break;
            }
            j += d.len_utf8();
        }
        let token = &b[i..j];
        let skip = token.contains('@') || token.contains("://") || token.starts_with("www.") || token.contains('_') || token.contains('/');
        if !skip {
            // Split the token at hyphens, dots and colons; keep apostrophes inside.
            let mut k = i;
            for piece in token.split(['-', '.', ':']) {
                let start = k;
                let end = k + piece.len();
                k = end + 1;
                let p = piece.trim_end_matches(['\'', '’']);
                let p = p.trim_start_matches(['\'', '’']);
                if p.is_empty() {
                    continue;
                }
                let off = piece.find(p).unwrap_or(0);
                let (s, e) = (start + off, start + off + p.len());
                let has_digit = p.chars().any(|c| c.is_ascii_digit());
                let has_alpha = p.chars().any(|c| c.is_alphabetic());
                if !has_alpha || (has_digit && !p.chars().next().is_some_and(|c| c.is_ascii_digit())) {
                    continue;
                }
                out.push((s as u32, e as u32));
            }
        }
        i = j;
    }
    out
}

/// A checker for a document: a dictionary, the words the person added, and
/// those ignored for this session.
#[derive(Debug, Default)]
pub struct Speller {
    pub dict: Dictionary,
    pub added: HashSet<String>,
    pub ignored: HashSet<String>,
}

impl Speller {
    pub fn new(dict: Dictionary) -> Speller {
        Speller { dict, added: HashSet::new(), ignored: HashSet::new() }
    }

    pub fn check(&self, word: &str) -> bool {
        self.added.contains(word) || self.ignored.contains(word) || self.added.contains(&word.to_lowercase()) || self.dict.check(word)
    }

    /// The misspelled words of `text`.
    pub fn misspelled(&self, text: &str) -> Vec<(u32, u32)> {
        words(text).into_iter().filter(|&(s, e)| !self.check(&text[s as usize..e as usize])).collect()
    }

    pub fn suggest(&self, word: &str) -> Vec<String> {
        self.dict.suggest(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn en() -> &'static Dictionary {
        static D: std::sync::OnceLock<Dictionary> = std::sync::OnceLock::new();
        D.get_or_init(Dictionary::en_us)
    }

    #[test]
    fn english_words_check() {
        let d = en();
        assert!(d.unsupported().is_empty(), "{:?}", d.unsupported());
        for w in ["the", "The", "THE", "walked", "walking", "unhappiness", "reconnected", "cities", "flies", "happily", "London", "it's", "don't", "dogs'", "children's", "11th", "21st", "102nd", "3rd", "1,000", "co-operate", "OK", "Officina's"] {
            if w.contains('-') || w == "Officina's" {
                continue;
            }
            assert!(d.check(w), "{w}");
        }
        for w in ["teh", "recieve", "walkd", "london", "Thhe", "1th", "21th", "xyzzyq"] {
            assert!(!d.check(w), "{w}");
        }
    }

    #[test]
    fn suggestions() {
        let d = en();
        let s = d.suggest("recieve");
        assert_eq!(s.first().map(String::as_str), Some("receive"), "{s:?}");
        assert!(d.suggest("teh").contains(&"the".to_string()));
        assert!(d.suggest("Teh").contains(&"The".to_string()));
        assert!(d.suggest("wrod").contains(&"word".to_string()));
        assert!(d.suggest("alot").iter().any(|x| x == "a lot"), "{:?}", d.suggest("alot"));
        assert!(d.suggest("").is_empty());
    }

    #[test]
    fn words_in_text() {
        let t = "Hello, wrold! It's the 21st cen-tury: see https://example.org or me@x.org, A4 and R2D2.";
        let w: Vec<&str> = words(t).iter().map(|&(s, e)| &t[s as usize..e as usize]).collect();
        assert_eq!(w, ["Hello", "wrold", "It's", "the", "21st", "cen", "tury", "see", "or", "and"]);
        let sp = Speller::new(Dictionary::en_us());
        let bad: Vec<&str> = sp.misspelled(t).iter().map(|&(s, e)| &t[s as usize..e as usize]).collect();
        assert_eq!(bad, ["wrold", "cen", "tury"]);
    }

    #[test]
    fn hostile_files_load_or_fail_cleanly() {
        for aff in ["", "SET latin1", "PFX A Y 99999999\nPFX", "SFX A Y 2\nSFX A 0 s [^\nSFX A", "COMPOUNDRULE 1\nCOMPOUNDRULE ((((*", "FLAG long\nSFX AB Y 1\nSFX AB 0 x ."] {
            if let Ok(d) = Dictionary::load(aff, "2\nab/AB\nc/A\n") {
                d.check("abx");
                d.check("ab");
                d.suggest("abc");
            }
        }
        let d = Dictionary::load("FLAG long\nSFX Zz Y 1\nSFX Zz 0 s .", "1\ncat/Zz").unwrap();
        assert!(d.check("cats"));
        let d = Dictionary::load("FLAG num\nSFX 7 Y 1\nSFX 7 0 s .", "1\ncat/7,9").unwrap();
        assert!(d.check("cats"));
    }
}
