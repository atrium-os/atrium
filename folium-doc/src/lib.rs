//! Folium's document layer (docs/spec/folium.md §5): what a viewer needs
//! before it draws anything. The page tree is walked once at open (only
//! the tree's nodes are parsed, not the pages' content); everything else
//! is read when asked for. Hostile structure (loops, shared kids, absurd
//! counts) is cut at the budgets below, never followed forever.

pub mod text;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use folium_syntax::{Dict, Document, Error, Object, Ref, Result, Source, Stream};

/// Pages in one document.
pub const PAGES_MAX: usize = 1 << 20;
/// Page tree nesting.
pub const TREE_DEPTH_MAX: usize = 64;
/// Outline items read.
pub const OUTLINE_MAX: usize = 1 << 17;
/// Name and number tree nesting.
pub const NAME_TREE_DEPTH_MAX: usize = 32;
/// Letter, when a page has no usable `/MediaBox`.
pub const DEFAULT_BOX: Rect = Rect { x0: 0.0, y0: 0.0, x1: 612.0, y1: 792.0 };

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn width(&self) -> f64 { self.x1 - self.x0 }
    pub fn height(&self) -> f64 { self.y1 - self.y0 }
    fn from(o: &Object) -> Option<Rect> {
        let a = o.as_array()?;
        if a.len() != 4 {
            return None;
        }
        let v: Vec<f64> = a.iter().map(Object::as_f64).collect::<Option<_>>()?;
        let r = Rect { x0: v[0].min(v[2]), y0: v[1].min(v[3]), x1: v[0].max(v[2]), y1: v[1].max(v[3]) };
        (r.width() > 0.0 && r.height() > 0.0 && v.iter().all(|x| x.abs() < 1e7)).then_some(r)
    }
    fn intersect(&self, o: &Rect) -> Option<Rect> {
        let r = Rect { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) };
        (r.width() > 0.0 && r.height() > 0.0).then_some(r)
    }
}

/// What pages inherit from the tree's nodes (§7.7.3.4).
#[derive(Debug, Clone, Default)]
struct Inherited {
    resources: Option<Object>,
    media_box: Option<Object>,
    crop_box: Option<Object>,
    rotate: Option<Object>,
}

impl Inherited {
    fn over(&self, d: &Dict) -> Inherited {
        let pick = |k: &str, old: &Option<Object>| d.get(k).cloned().or_else(|| old.clone());
        Inherited {
            resources: pick("Resources", &self.resources),
            media_box: pick("MediaBox", &self.media_box),
            crop_box: pick("CropBox", &self.crop_box),
            rotate: pick("Rotate", &self.rotate),
        }
    }
}

struct PageRef {
    r: Option<Ref>,
    /// A page given directly (not by reference) in `/Kids`.
    direct: Option<Arc<Dict>>,
    inherited: Arc<Inherited>,
}

/// A page, ready for the content layer.
#[derive(Debug, Clone)]
pub struct Page {
    pub index: usize,
    pub r: Option<Ref>,
    pub dict: Dict,
    pub media_box: Rect,
    /// The crop box, within the media box; what is shown.
    pub crop_box: Rect,
    /// 0, 90, 180 or 270, clockwise.
    pub rotate: u16,
    /// Default user space unit in 1/72 inch (§14.11.2.2).
    pub user_unit: f64,
    pub resources: Dict,
}

impl Page {
    /// Shown size in points, rotation and user unit applied.
    pub fn size(&self) -> (f64, f64) {
        let (w, h) = (self.crop_box.width() * self.user_unit, self.crop_box.height() * self.user_unit);
        if self.rotate % 180 == 90 { (h, w) } else { (w, h) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Fit {
    Xyz { left: Option<f64>, top: Option<f64>, zoom: Option<f64> },
    Fit,
    FitH(Option<f64>),
    FitV(Option<f64>),
    FitR(Rect),
    FitB,
    FitBH(Option<f64>),
    FitBV(Option<f64>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Page { index: usize, fit: Fit },
    Uri(String),
    /// A destination in another file, or an action this layer doesn't
    /// follow (named actions, JavaScript, launch: never run, §8).
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub target: Option<Target>,
    pub open: bool,
    pub children: Vec<OutlineItem>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
}

pub struct Doc {
    pub syntax: Document,
    catalog: Dict,
    pages: Vec<PageRef>,
    page_index: HashMap<Ref, usize>,
    /// The page tree was damaged and was read as far as it could be.
    pub tree_damaged: bool,
}

impl Doc {
    pub fn open(src: Box<dyn Source>, password: &[u8]) -> Result<Doc> {
        Self::from_syntax(Document::open_with_password(src, password)?)
    }

    pub fn from_syntax(syntax: Document) -> Result<Doc> {
        let catalog = syntax.catalog()?;
        let mut d = Doc { syntax, catalog, pages: Vec::new(), page_index: HashMap::new(), tree_damaged: false };
        d.walk()?;
        Ok(d)
    }

    pub fn catalog(&self) -> &Dict { &self.catalog }
    pub fn page_count(&self) -> usize { self.pages.len() }
    pub fn page_of(&self, r: Ref) -> Option<usize> { self.page_index.get(&r).copied() }

    fn walk(&mut self) -> Result<()> {
        let root = self.catalog.get("Pages").cloned().unwrap_or(Object::Null);
        let mut seen: HashSet<Ref> = HashSet::new();
        // An explicit stack: (node, inherited, depth).
        let mut stack: Vec<(Object, Arc<Inherited>, usize)> = vec![(root, Arc::new(Inherited::default()), 0)];
        while let Some((node, inh, depth)) = stack.pop() {
            if self.pages.len() >= PAGES_MAX {
                return Err(Error::Limit("pages in one document"));
            }
            let r = node.as_ref();
            if let Some(r) = r {
                if !seen.insert(r) {
                    self.tree_damaged = true;
                    continue;
                }
            }
            let o = match self.syntax.resolve(&node) {
                Ok(o) => o,
                Err(Error::Limit(w)) => return Err(Error::Limit(w)),
                Err(_) => {
                    self.tree_damaged = true;
                    continue;
                }
            };
            let Some(dict) = o.as_dict() else {
                self.tree_damaged = true;
                continue;
            };
            let kids = match dict.get("Kids") {
                Some(k) => self.syntax.resolve(k).ok(),
                None => None,
            };
            let is_page = dict.type_is("Page") || (!dict.type_is("Pages") && kids.is_none());
            if is_page {
                if let Some(r) = r {
                    self.page_index.entry(r).or_insert(self.pages.len());
                }
                let direct = r.is_none().then(|| Arc::new(dict.clone()));
                self.pages.push(PageRef { r, direct, inherited: inh });
                continue;
            }
            if depth >= TREE_DEPTH_MAX {
                self.tree_damaged = true;
                continue;
            }
            let inh = Arc::new(inh.over(dict));
            if let Some(Object::Array(kids)) = kids {
                for k in kids.into_iter().rev() {
                    stack.push((k, inh.clone(), depth + 1));
                }
            } else {
                self.tree_damaged = true;
            }
        }
        if self.pages.is_empty() {
            return Err(Error::Unreadable("no pages"));
        }
        Ok(())
    }

    pub fn page(&self, index: usize) -> Result<Page> {
        let p = self.pages.get(index).ok_or(Error::Unreadable("a page past the last"))?;
        let dict = match (&p.direct, p.r) {
            (Some(d), _) => (**d).clone(),
            (None, Some(r)) => self.syntax.get(r)?.as_dict().cloned().unwrap_or_default(),
            (None, None) => Dict::new(),
        };
        let inh = p.inherited.over(&dict);
        let rect = |o: &Option<Object>| o.as_ref().and_then(|o| self.syntax.resolve(o).ok()).and_then(|o| Rect::from(&o));
        let media_box = rect(&inh.media_box).unwrap_or(DEFAULT_BOX);
        let crop_box = rect(&inh.crop_box).and_then(|c| c.intersect(&media_box)).unwrap_or(media_box);
        let rotate = inh
            .rotate
            .as_ref()
            .and_then(|o| self.syntax.resolve(o).ok())
            .and_then(|o| o.as_int())
            .map(|r| (r.rem_euclid(360) / 90 * 90) as u16)
            .unwrap_or(0);
        let user_unit = self
            .syntax
            .get_in(&dict, "UserUnit")
            .ok()
            .and_then(|o| o.as_f64())
            .filter(|u| *u > 0.0 && *u <= 75_000.0)
            .unwrap_or(1.0);
        let resources = inh
            .resources
            .as_ref()
            .and_then(|o| self.syntax.resolve(o).ok())
            .and_then(|o| o.as_dict().cloned())
            .unwrap_or_default();
        Ok(Page { index, r: p.r, dict, media_box, crop_box, rotate, user_unit, resources })
    }

    /// The page's content streams, in order (a single stream or an array).
    pub fn contents(&self, page: &Page) -> Result<Vec<Stream>> {
        let c = self.syntax.get_in(&page.dict, "Contents")?;
        let items: Vec<Object> = match c {
            Object::Array(a) => a,
            o => vec![o],
        };
        let mut v = Vec::new();
        for o in items {
            if let Object::Stream(s) = self.syntax.resolve(&o)? {
                v.push(s);
            }
        }
        Ok(v)
    }

    /// Document info: the trailer's `/Info` (XMP is read in P4).
    pub fn info(&self) -> Info {
        let Ok(Object::Dict(d)) = self.syntax.get_in(self.syntax.trailer(), "Info") else { return Info::default() };
        let s = |k: &str| match self.syntax.get_in(&d, k) {
            Ok(Object::String(s)) => Some(text::decode(&s)).filter(|t| !t.trim().is_empty()),
            _ => None,
        };
        Info {
            title: s("Title"),
            author: s("Author"),
            subject: s("Subject"),
            keywords: s("Keywords"),
            creator: s("Creator"),
            producer: s("Producer"),
            created: s("CreationDate"),
            modified: s("ModDate"),
        }
    }

    /// The outline (bookmarks), cut at [`OUTLINE_MAX`] items.
    pub fn outline(&self) -> Vec<OutlineItem> {
        let Ok(Object::Dict(root)) = self.syntax.get_in(&self.catalog, "Outlines") else { return Vec::new() };
        let mut seen = HashSet::new();
        let mut budget = OUTLINE_MAX;
        self.outline_level(root.get("First"), &mut seen, &mut budget, 0)
    }

    fn outline_level(&self, first: Option<&Object>, seen: &mut HashSet<Ref>, budget: &mut usize, depth: usize) -> Vec<OutlineItem> {
        let mut out = Vec::new();
        if depth > TREE_DEPTH_MAX {
            return out;
        }
        let mut cur = first.cloned();
        while let Some(o) = cur.take() {
            if *budget == 0 {
                break;
            }
            if let Some(r) = o.as_ref() {
                if !seen.insert(r) {
                    break;
                }
            }
            let Ok(Object::Dict(d)) = self.syntax.resolve(&o) else { break };
            *budget -= 1;
            let title = match self.syntax.get_in(&d, "Title") {
                Ok(Object::String(s)) => text::decode(&s),
                _ => String::new(),
            };
            let target = self.target_of(&d);
            let open = self.syntax.get_in(&d, "Count").ok().and_then(|c| c.as_int()).is_some_and(|c| c > 0);
            let children = self.outline_level(d.get("First"), seen, budget, depth + 1);
            out.push(OutlineItem { title, target, open, children });
            cur = d.get("Next").cloned();
        }
        out
    }

    /// Where an outline item or link annotation goes: `/Dest`, else `/A`.
    pub fn target_of(&self, d: &Dict) -> Option<Target> {
        if let Some(dest) = d.get("Dest") {
            return self.dest(dest);
        }
        let Ok(Object::Dict(a)) = self.syntax.get_in(d, "A") else { return None };
        let s = a.name("S")?;
        if s.is("GoTo") {
            self.dest(a.get("D")?)
        } else if s.is("URI") {
            match self.syntax.get_in(&a, "URI") {
                Ok(Object::String(u)) => Some(Target::Uri(String::from_utf8_lossy(&u).into_owned())),
                _ => None,
            }
        } else {
            Some(Target::Other(String::from_utf8_lossy(s.as_bytes()).into_owned()))
        }
    }

    /// A destination: an explicit array, or a name looked up in the
    /// catalog's `/Dests` or the `/Names` `/Dests` tree.
    pub fn dest(&self, o: &Object) -> Option<Target> {
        let o = self.syntax.resolve(o).ok()?;
        let arr = match &o {
            Object::Array(a) => a.clone(),
            Object::Name(n) => self.named_dest(n.as_bytes())?,
            Object::String(s) => self.named_dest(s)?,
            Object::Dict(d) => return self.dest(d.get("D")?),
            _ => return None,
        };
        let page = arr.first()?;
        let index = match page {
            Object::Ref(r) => self.page_of(*r)?,
            // Remote-style page numbers inside a local GoTo: tolerated.
            o => o.as_int().filter(|i| *i >= 0 && (*i as usize) < self.pages.len())? as usize,
        };
        let num = |i: usize| arr.get(i).and_then(|o| self.syntax.resolve(o).ok()).and_then(|o| o.as_f64());
        let kind = arr.get(1).and_then(Object::as_name).map(|n| n.as_bytes().to_vec()).unwrap_or_default();
        let fit = match &kind[..] {
            b"Fit" => Fit::Fit,
            b"FitH" => Fit::FitH(num(2)),
            b"FitV" => Fit::FitV(num(2)),
            b"FitB" => Fit::FitB,
            b"FitBH" => Fit::FitBH(num(2)),
            b"FitBV" => Fit::FitBV(num(2)),
            b"FitR" => match (num(2), num(3), num(4), num(5)) {
                (Some(a), Some(b), Some(c), Some(d)) => {
                    Fit::FitR(Rect { x0: a.min(c), y0: b.min(d), x1: a.max(c), y1: b.max(d) })
                }
                _ => Fit::Fit,
            },
            _ => Fit::Xyz { left: num(2), top: num(3), zoom: num(4).filter(|z| *z > 0.0) },
        };
        Some(Target::Page { index, fit })
    }

    fn named_dest(&self, name: &[u8]) -> Option<Vec<Object>> {
        let hit = if let Ok(Object::Dict(d)) = self.syntax.get_in(&self.catalog, "Dests") {
            d.iter().find(|(k, _)| k.as_bytes() == name).map(|(_, v)| v.clone())
        } else {
            None
        };
        let hit = hit.or_else(|| {
            let Ok(Object::Dict(names)) = self.syntax.get_in(&self.catalog, "Names") else { return None };
            let tree = names.get("Dests")?;
            self.name_tree_lookup(tree, name, 0)
        })?;
        match self.syntax.resolve(&hit).ok()? {
            Object::Array(a) => Some(a),
            Object::Dict(d) => self.syntax.get_in(&d, "D").ok()?.as_array().map(|a| a.to_vec()),
            _ => None,
        }
    }

    /// Look `key` up in a name tree (§7.9.6), using `/Limits` to choose
    /// the kid and falling back to a scan when limits are missing.
    pub fn name_tree_lookup(&self, node: &Object, key: &[u8], depth: usize) -> Option<Object> {
        if depth > NAME_TREE_DEPTH_MAX {
            return None;
        }
        let Object::Dict(d) = self.syntax.resolve(node).ok()? else { return None };
        if let Ok(Object::Array(names)) = self.syntax.get_in(&d, "Names") {
            for pair in names.chunks_exact(2) {
                let k = self.syntax.resolve(&pair[0]).ok()?;
                if k.as_str() == Some(key) {
                    return Some(pair[1].clone());
                }
            }
        }
        if let Ok(Object::Array(kids)) = self.syntax.get_in(&d, "Kids") {
            for k in &kids {
                let Ok(Object::Dict(kd)) = self.syntax.resolve(k) else { continue };
                if let Ok(Object::Array(l)) = self.syntax.get_in(&kd, "Limits") {
                    if let (Some(lo), Some(hi)) = (l.first().and_then(Object::as_str), l.get(1).and_then(Object::as_str)) {
                        if key < lo || key > hi {
                            continue;
                        }
                    }
                }
                if let Some(v) = self.name_tree_lookup(k, key, depth + 1) {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Page labels (§12.4.2) for every page; `None` when the document
    /// has none (show 1, 2, 3 …).
    pub fn page_labels(&self) -> Option<Vec<String>> {
        let tree = self.catalog.get("PageLabels")?;
        let mut ranges: Vec<(usize, Dict)> = Vec::new();
        self.number_tree(tree, &mut ranges, 0);
        if ranges.is_empty() {
            return None;
        }
        ranges.sort_by_key(|r| r.0);
        let n = self.pages.len();
        let mut out = Vec::with_capacity(n);
        for (i, (start, d)) in ranges.iter().enumerate() {
            let end = ranges.get(i + 1).map(|r| r.0).unwrap_or(n).min(n);
            let style = d.name("S").map(|s| s.as_bytes().to_vec());
            let prefix = match self.syntax.get_in(d, "P") {
                Ok(Object::String(s)) => text::decode(&s),
                _ => String::new(),
            };
            let first = self.syntax.get_in(d, "St").ok().and_then(|o| o.as_int()).filter(|s| *s >= 1).unwrap_or(1);
            while out.len() < *start.min(&n) {
                out.push((out.len() + 1).to_string());
            }
            for k in out.len()..end {
                let v = first + (k - start) as i64;
                let num = match style.as_deref() {
                    Some(b"D") => v.to_string(),
                    Some(b"R") => roman(v),
                    Some(b"r") => roman(v).to_lowercase(),
                    Some(b"A") => letters(v),
                    Some(b"a") => letters(v).to_lowercase(),
                    _ => String::new(),
                };
                out.push(format!("{prefix}{num}"));
            }
        }
        while out.len() < n {
            out.push((out.len() + 1).to_string());
        }
        Some(out)
    }

    fn number_tree(&self, node: &Object, out: &mut Vec<(usize, Dict)>, depth: usize) {
        if depth > NAME_TREE_DEPTH_MAX || out.len() > PAGES_MAX {
            return;
        }
        let Ok(Object::Dict(d)) = self.syntax.resolve(node) else { return };
        if let Ok(Object::Array(nums)) = self.syntax.get_in(&d, "Nums") {
            for pair in nums.chunks_exact(2) {
                let (Some(k), Ok(Object::Dict(v))) = (pair[0].as_int(), self.syntax.resolve(&pair[1])) else { continue };
                if k >= 0 {
                    out.push((k as usize, v));
                }
            }
        }
        if let Ok(Object::Array(kids)) = self.syntax.get_in(&d, "Kids") {
            for k in &kids {
                self.number_tree(k, out, depth + 1);
            }
        }
    }
}

fn roman(mut v: i64) -> String {
    if !(1..4000).contains(&v) {
        return v.to_string();
    }
    let t = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")];
    let mut s = String::new();
    for (n, r) in t {
        while v >= n {
            s += r;
            v -= n;
        }
    }
    s
}

/// A, B, … Z, AA, BB, … (§12.4.2).
fn letters(v: i64) -> String {
    if v < 1 {
        return String::new();
    }
    let v = v.min(26 * 1000);
    let c = (b'A' + ((v - 1) % 26) as u8) as char;
    std::iter::repeat_n(c, ((v - 1) / 26 + 1) as usize).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(roman(1994), "MCMXCIV");
        assert_eq!(letters(1), "A");
        assert_eq!(letters(28), "BB");
    }
}
