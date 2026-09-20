//! A DOM as an arena of nodes addressed by integer handles.
//!
//! Handles rather than pointers is deliberate: script holds only a `u32`, so
//! every mutation crosses one host boundary where it can be counted and
//! bounded, and a stale handle is a range check rather than a dangling
//! reference. It is also the shape a memory-safe engine would want later
//! (see the spec's §11.8 on arena + generational indices).

use std::collections::HashMap;

pub type Handle = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Document,
    Element(String),
    Text(String),
    Comment(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: Kind,
    pub attrs: Vec<(String, String)>,
    pub parent: Option<Handle>,
    pub children: Vec<Handle>,
}

#[derive(Debug, Default)]
pub struct Dom {
    pub nodes: Vec<Node>,
    /// Mutations applied by script, the number this instrument exists to report.
    pub script_mutations: u64,
}

impl Dom {
    pub fn new() -> Self {
        let mut d = Dom::default();
        d.nodes.push(Node { kind: Kind::Document, attrs: vec![], parent: None, children: vec![] });
        d
    }
    pub fn root(&self) -> Handle { 0 }
    pub fn get(&self, h: Handle) -> Option<&Node> { self.nodes.get(h as usize) }
    pub fn get_mut(&mut self, h: Handle) -> Option<&mut Node> { self.nodes.get_mut(h as usize) }

    pub fn create(&mut self, kind: Kind) -> Handle {
        self.nodes.push(Node { kind, attrs: vec![], parent: None, children: vec![] });
        (self.nodes.len() - 1) as Handle
    }

    pub fn append(&mut self, parent: Handle, child: Handle) -> bool {
        if parent as usize >= self.nodes.len() || child as usize >= self.nodes.len() { return false; }
        if parent == child || self.is_ancestor(child, parent) { return false; } // no cycles
        if let Some(old) = self.nodes[child as usize].parent {
            self.nodes[old as usize].children.retain(|&c| c != child);
        }
        self.nodes[child as usize].parent = Some(parent);
        self.nodes[parent as usize].children.push(child);
        true
    }

    fn is_ancestor(&self, maybe_anc: Handle, of: Handle) -> bool {
        let mut cur = self.nodes[of as usize].parent;
        while let Some(p) = cur {
            if p == maybe_anc { return true; }
            cur = self.nodes[p as usize].parent;
        }
        false
    }

    pub fn tag(&self, h: Handle) -> Option<&str> {
        match &self.get(h)?.kind { Kind::Element(t) => Some(t), _ => None }
    }

    pub fn attr(&self, h: Handle, name: &str) -> Option<&str> {
        self.get(h)?.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn set_attr(&mut self, h: Handle, name: &str, val: &str) {
        if let Some(n) = self.get_mut(h) {
            if let Some(a) = n.attrs.iter_mut().find(|(k, _)| k == name) { a.1 = val.to_string(); }
            else { n.attrs.push((name.to_string(), val.to_string())); }
        }
    }

    pub fn by_id(&self, id: &str) -> Option<Handle> {
        (0..self.nodes.len() as Handle).find(|&h| self.attr(h, "id") == Some(id))
    }

    pub fn by_tag(&self, tag: &str) -> Vec<Handle> {
        (0..self.nodes.len() as Handle)
            .filter(|&h| self.tag(h).map(|t| t.eq_ignore_ascii_case(tag)).unwrap_or(false))
            .collect()
    }

    pub fn text_content(&self, h: Handle) -> String {
        let mut out = String::new();
        self.walk_text(h, &mut out);
        out
    }
    fn walk_text(&self, h: Handle, out: &mut String) {
        match &self.nodes[h as usize].kind {
            Kind::Text(t) => out.push_str(t),
            _ => for &c in &self.nodes[h as usize].children { self.walk_text(c, out); },
        }
    }

    pub fn set_text(&mut self, h: Handle, text: &str) {
        if let Some(n) = self.get_mut(h) { n.children.clear(); }
        let t = self.create(Kind::Text(text.to_string()));
        self.append(h, t);
    }

    /// Element count, the headline metric for a conversion.
    pub fn element_count(&self) -> usize {
        self.nodes.iter().filter(|n| matches!(n.kind, Kind::Element(_))).count()
    }

    pub fn max_depth(&self) -> usize {
        fn go(d: &Dom, h: Handle, cur: usize, best: &mut usize) {
            *best = (*best).max(cur);
            for &c in &d.nodes[h as usize].children { go(d, c, cur + 1, best); }
        }
        let mut best = 0;
        go(self, self.root(), 0, &mut best);
        best
    }

    pub fn serialize(&self) -> String {
        let mut s = String::new();
        for &c in &self.nodes[self.root() as usize].children { self.ser(c, &mut s); }
        s
    }
    fn ser(&self, h: Handle, s: &mut String) {
        const VOID: &[&str] = &["area","base","br","col","embed","hr","img","input","link","meta","source","track","wbr"];
        match &self.nodes[h as usize].kind {
            Kind::Text(t) => s.push_str(&escape(t)),
            Kind::Comment(_) => {}
            Kind::Document => {}
            Kind::Element(tag) => {
                s.push('<'); s.push_str(tag);
                for (k, v) in &self.nodes[h as usize].attrs {
                    s.push(' '); s.push_str(k); s.push_str("=\""); s.push_str(&escape(v)); s.push('"');
                }
                s.push('>');
                if VOID.contains(&tag.as_str()) { return; }
                for &c in &self.nodes[h as usize].children { self.ser(c, s); }
                s.push_str("</"); s.push_str(tag); s.push('>');
            }
        }
    }
}

fn escape(t: &str) -> String {
    t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Inline scripts in document order.
///
/// ★ The `type` filter is load-bearing and was missing at first: a real corpus
/// run reported 9 `SyntaxError: expected token ';'` because every `<script>`
/// without `src` was being executed — including `application/ld+json`
/// metadata blocks, which are data, not programs. Feeding those to the engine
/// manufactures failures that look like engine gaps and are nothing of the
/// kind. Only script types that are actually JavaScript are run.
pub fn inline_scripts(d: &Dom) -> Vec<String> {
    d.by_tag("script").into_iter()
        .filter(|&h| d.attr(h, "src").is_none())
        .filter(|&h| match d.attr(h, "type") {
            None => true,
            Some(t) => {
                let t = t.trim().to_ascii_lowercase();
                let t = t.split(';').next().unwrap_or("").trim().to_string();
                matches!(t.as_str(),
                    "" | "text/javascript" | "application/javascript"
                    | "text/ecmascript" | "application/ecmascript" | "module")
            }
        })
        .map(|h| d.text_content(h))
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Script elements present but NOT run, by type — what a conversion skipped.
pub fn skipped_script_types(d: &Dom) -> Vec<String> {
    d.by_tag("script").into_iter()
        .filter(|&h| d.attr(h, "src").is_none())
        .filter_map(|h| d.attr(h, "type").map(|t| t.to_string()))
        .filter(|t| {
            let t = t.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            !matches!(t.as_str(),
                "" | "text/javascript" | "application/javascript"
                | "text/ecmascript" | "application/ecmascript" | "module")
        })
        .collect()
}

pub fn id_index(d: &Dom) -> HashMap<String, Handle> {
    let mut m = HashMap::new();
    for h in 0..d.nodes.len() as Handle {
        if let Some(id) = d.attr(h, "id") { m.insert(id.to_string(), h); }
    }
    m
}
