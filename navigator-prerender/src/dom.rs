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
    /// A DocumentFragment: a parentless holder whose CHILDREN are what gets
    /// inserted. jQuery builds every template through one.
    Fragment,
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
    /// ★ EXPERIMENT: protected-subtree execution.
    ///
    /// Parser-produced nodes occupy handles `0..parser_nodes`, because the
    /// arena hands them out in order and every later node was created by
    /// script. When protection is on, a script may ADD, decorate and reorder,
    /// but may not REMOVE what the server sent — making hydration
    /// non-destructive by construction instead of detecting the damage
    /// afterwards.
    pub protect_parser_nodes: bool,
    pub parser_nodes: Handle,
    /// Removals refused by that rule, so the cost is measured not assumed.
    pub removals_refused: u32,
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

    /// ★ IS THIS NODE STILL IN THE DOCUMENT?
    ///
    /// The arena keeps detached nodes — one removed, or created and never
    /// inserted — and document-wide lookups scanned ALL of them. So
    /// `document.getElementById('x')` still found an element after
    /// `replaceWith` removed it, and a `createElement('form')` that was never
    /// inserted showed up in `document.forms`. A document-level query has to
    /// mean "in the document".
    pub fn connected(&self, h: Handle) -> bool {
        let mut cur = Some(h);
        let mut guard = 0;
        while let Some(n) = cur {
            if n == self.root() { return true }
            guard += 1;
            if guard > 10_000 { return false }
            cur = self.get(n).and_then(|x| x.parent);
        }
        false
    }

    pub fn by_id(&self, id: &str) -> Option<Handle> {
        (0..self.nodes.len() as Handle)
            .find(|&h| self.attr(h, "id") == Some(id) && self.connected(h))
    }
    pub fn by_tag(&self, tag: &str) -> Vec<Handle> {
        self.by_tag_anywhere(tag).into_iter().filter(|&h| self.connected(h)).collect()
    }

    /// Every element with the tag, CONNECTED OR NOT — for the few callers
    /// that mean the arena rather than the document.
    pub fn by_tag_anywhere(&self, tag: &str) -> Vec<Handle> {
        (0..self.nodes.len() as Handle)
            .filter(|&h| self.tag(h).map(|t| t.eq_ignore_ascii_case(tag)).unwrap_or(false))
            .collect()
    }

    /// ★ VISIBLE text: what a READER would see.
    ///
    /// `text_content` descends into everything, including `<script>` and
    /// `<style>`, whose text nodes hold source code. Measuring content gain
    /// with it measured JAVASCRIPT — on bbc.co.uk it reported a 9,698
    /// character LOSS while the visible words went 2,829 -> 2,832 with none
    /// missing. A metric that counts the program as content will rank a page
    /// by how much script it ships.
    pub fn visible_text(&self, h: Handle) -> String {
        let mut out = String::new();
        self.walk_visible(h, &mut out);
        out
    }
    fn walk_visible(&self, h: Handle, out: &mut String) {
        match &self.nodes[h as usize].kind {
            Kind::Text(t) => out.push_str(t),
            Kind::Element(tag) if matches!(tag.to_ascii_lowercase().as_str(),
                "script" | "style" | "noscript" | "template" | "title") => {}
            _ => for &c in &self.nodes[h as usize].children { self.walk_visible(c, out) },
        }
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
        // Replacing an element's text REMOVES its children. If any of them
        // are the server's, that is a destruction and the whole assignment
        // is refused — a partial one would leave the page in a state neither
        // it nor we intended.
        if self.protect_parser_nodes {
            let doomed = self.get(h).map(|n| n.children.clone()).unwrap_or_default();
            if doomed.iter().any(|&c| self.is_protected(c)) {
                self.removals_refused += 1;
                return;
            }
        }
        if let Some(n) = self.get_mut(h) { n.children.clear(); }
        let t = self.create(Kind::Text(text.to_string()));
        self.append(h, t);
    }

    // ── Tree semantics ──────────────────────────────────────────────
    //
    // ★ The arena ALWAYS had the tree: `parent` and `children` were populated
    // from the start. What was missing was every way to READ or RESHAPE it
    // from script, which made this a write-mostly facade — build-only, no
    // navigation, no cloning. Any library that feature-detects the DOM died
    // on contact: jQuery gets four lines into its Sizzle setup and stops.
    // These are the operations that make it a DOM rather than a builder.

    /// DOM nodeType, the number libraries branch on. `document` must report 9
    /// or jQuery's setDocument() bails and leaves its own document undefined.
    pub fn node_type(&self, h: Handle) -> u32 {
        match self.get(h).map(|n| &n.kind) {
            Some(Kind::Element(_)) => 1,
            Some(Kind::Text(_)) => 3,
            Some(Kind::Comment(_)) => 8,
            Some(Kind::Document) => 9,
            Some(Kind::Fragment) => 11,
            None => 0,
        }
    }

    pub fn node_name(&self, h: Handle) -> String {
        match self.get(h).map(|n| &n.kind) {
            Some(Kind::Element(t)) => t.to_uppercase(),
            Some(Kind::Text(_)) => "#text".into(),
            Some(Kind::Comment(_)) => "#comment".into(),
            Some(Kind::Document) => "#document".into(),
            Some(Kind::Fragment) => "#document-fragment".into(),
            None => String::new(),
        }
    }

    pub fn children_of(&self, h: Handle) -> Vec<Handle> {
        self.get(h).map(|n| n.children.clone()).unwrap_or_default()
    }

    /// Element children only — `children` in the DOM, as against `childNodes`.
    pub fn element_children(&self, h: Handle) -> Vec<Handle> {
        self.children_of(h).into_iter()
            .filter(|&c| matches!(self.get(c).map(|n| &n.kind), Some(Kind::Element(_))))
            .collect()
    }

    pub fn first_child(&self, h: Handle) -> Option<Handle> {
        self.get(h)?.children.first().copied()
    }
    pub fn last_child(&self, h: Handle) -> Option<Handle> {
        self.get(h)?.children.last().copied()
    }

    fn index_in_parent(&self, h: Handle) -> Option<(Handle, usize)> {
        let p = self.get(h)?.parent?;
        let i = self.get(p)?.children.iter().position(|&c| c == h)?;
        Some((p, i))
    }

    pub fn next_sibling(&self, h: Handle) -> Option<Handle> {
        let (p, i) = self.index_in_parent(h)?;
        self.get(p)?.children.get(i + 1).copied()
    }
    pub fn previous_sibling(&self, h: Handle) -> Option<Handle> {
        let (p, i) = self.index_in_parent(h)?;
        if i == 0 { return None }
        self.get(p)?.children.get(i - 1).copied()
    }

    /// Is this node the server's rather than the script's?
    pub fn is_protected(&self, h: Handle) -> bool {
        self.protect_parser_nodes && h < self.parser_nodes
    }

    /// Detach as a REMOVAL — subject to protection.
    pub fn detach(&mut self, h: Handle) -> bool {
        if self.is_protected(h) { self.removals_refused += 1; return false }
        self.detach_for_move(h)
    }

    /// Detach as part of a MOVE. Never protected: the node stays in the
    /// document, and blocking reordering would break pages that legitimately
    /// rearrange the server's own markup without destroying it.
    pub fn detach_for_move(&mut self, h: Handle) -> bool {
        let Some(p) = self.get(h).and_then(|n| n.parent) else { return false };
        if let Some(n) = self.get_mut(p) { n.children.retain(|&c| c != h); }
        if let Some(n) = self.get_mut(h) { n.parent = None; }
        true
    }

    /// `insertBefore(new, ref)`. A null `before` appends, as the DOM says.
    /// A fragment inserts its CHILDREN, not itself — the behaviour every
    /// template-building library depends on.
    pub fn insert_before(&mut self, parent: Handle, node: Handle, before: Option<Handle>) -> bool {
        if parent as usize >= self.nodes.len() || node as usize >= self.nodes.len() { return false }
        if parent == node || self.is_ancestor(node, parent) { return false }
        if matches!(self.nodes[node as usize].kind, Kind::Fragment) {
            let kids = self.children_of(node);
            let mut ok = true;
            for k in kids { ok &= self.insert_before(parent, k, before); }
            return ok;
        }
        self.detach_for_move(node);
        let at = match before {
            Some(b) => self.get(parent).and_then(|n| n.children.iter().position(|&c| c == b))
                .unwrap_or_else(|| self.nodes[parent as usize].children.len()),
            None => self.nodes[parent as usize].children.len(),
        };
        self.nodes[node as usize].parent = Some(parent);
        self.nodes[parent as usize].children.insert(at, node);
        true
    }

    pub fn remove_child(&mut self, parent: Handle, child: Handle) -> bool {
        if self.get(child).and_then(|n| n.parent) != Some(parent) { return false }
        self.detach(child)
    }

    pub fn replace_child(&mut self, parent: Handle, new: Handle, old: Handle) -> bool {
        let Some(i) = self.get(parent).and_then(|n| n.children.iter().position(|&c| c == old))
            else { return false };
        if !self.insert_before(parent, new, Some(old)) { return false }
        let _ = i;
        self.remove_child(parent, old)
    }

    pub fn contains(&self, anc: Handle, h: Handle) -> bool {
        anc == h || self.is_ancestor(anc, h)
    }

    /// `cloneNode(deep)`. The clone is parentless, as the DOM requires — and
    /// deep must actually copy the subtree: jQuery reads
    /// `div.cloneNode(true).cloneNode(true).lastChild.checked`, so a clone
    /// that drops children throws on the very next property.
    pub fn clone_node(&mut self, h: Handle, deep: bool) -> Option<Handle> {
        let src = self.get(h)?.clone();
        let new = self.create(src.kind.clone());
        if let Some(n) = self.get_mut(new) { n.attrs = src.attrs.clone(); }
        if deep {
            for c in src.children {
                if let Some(cc) = self.clone_node(c, true) {
                    self.append(new, cc);
                }
            }
        }
        Some(new)
    }

    /// Serialize a node's children — `innerHTML` as read.
    pub fn inner_html(&self, h: Handle) -> String {
        let mut s = String::new();
        for &c in &self.get(h).map(|n| n.children.clone()).unwrap_or_default() { self.ser(c, &mut s); }
        s
    }
    /// Serialize the node itself — `outerHTML`.
    pub fn outer_html(&self, h: Handle) -> String {
        let mut s = String::new();
        self.ser(h, &mut s);
        s
    }

    /// Copy a subtree out of another arena, returning the new handle here.
    /// Used to graft parsed fragments in, so `innerHTML =` reuses the real
    /// parser rather than a second, divergent one.
    pub fn graft(&mut self, other: &Dom, from: Handle) -> Handle {
        let kind = other.get(from).map(|n| n.kind.clone()).unwrap_or(Kind::Fragment);
        let new = self.create(kind);
        if let Some(src) = other.get(from) {
            let attrs = src.attrs.clone();
            if let Some(n) = self.get_mut(new) { n.attrs = attrs; }
        }
        for &c in &other.get(from).map(|n| n.children.clone()).unwrap_or_default() {
            let cc = self.graft(other, c);
            self.append(new, cc);
        }
        new
    }

    // ── dataset ─────────────────────────────────────────────────────
    //
    // `data-*` attributes seen as properties. The name mapping is the whole
    // substance: `el.dataset.fooBar` is the attribute `data-foo-bar`, and a
    // dash followed by a letter uppercases it on the way back. Getting that
    // backwards silently reads the wrong attribute rather than failing.

    /// `fooBar` -> `data-foo-bar`.
    pub fn data_attr_name(prop: &str) -> String {
        let mut out = String::from("data-");
        for c in prop.chars() {
            if c.is_ascii_uppercase() { out.push('-'); out.push(c.to_ascii_lowercase()) }
            else { out.push(c) }
        }
        out
    }

    /// `data-foo-bar` -> `fooBar`, or None if it is not a data attribute.
    pub fn data_prop_name(attr: &str) -> Option<String> {
        let rest = attr.strip_prefix("data-")?;
        let mut out = String::new();
        let mut up = false;
        for c in rest.chars() {
            if c == '-' { up = true; continue }
            if up { out.extend(c.to_uppercase()); up = false } else { out.push(c) }
        }
        Some(out)
    }

    pub fn data_get(&self, h: Handle, prop: &str) -> Option<String> {
        self.attr(h, &Self::data_attr_name(prop)).map(str::to_string)
    }
    pub fn data_set(&mut self, h: Handle, prop: &str, val: &str) {
        let name = Self::data_attr_name(prop);
        self.set_attr(h, &name, val);
    }
    pub fn data_remove(&mut self, h: Handle, prop: &str) {
        let name = Self::data_attr_name(prop);
        if let Some(n) = self.get_mut(h) { n.attrs.retain(|(k, _)| k != &name) }
    }
    /// Every data-* property currently on the element, in attribute order.
    pub fn data_keys(&self, h: Handle) -> Vec<String> {
        self.get(h).map(|n| n.attrs.iter()
            .filter_map(|(k, _)| Self::data_prop_name(k))
            .collect()).unwrap_or_default()
    }

    /// Element count, the headline metric for a conversion.
    ///
    /// ★ CONNECTED elements only. Counting the arena reported bbc.co.uk as
    /// growing 4,184 -> 8,388 elements while its document was being emptied:
    /// the page had created thousands of nodes it never attached, and
    /// detached most of what the parser built. An "elements after" that
    /// counts orphans says a conversion grew when it shrank.
    pub fn element_count(&self) -> usize {
        (0..self.nodes.len() as Handle)
            .filter(|&h| matches!(self.nodes[h as usize].kind, Kind::Element(_)))
            .filter(|&h| self.connected(h))
            .count()
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
            Kind::Document | Kind::Fragment => {
                for &c in &self.nodes[h as usize].children { self.ser(c, s); }
            }
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

/// A script the document asks for, in document order.
#[derive(Debug, Clone, PartialEq)]
pub enum Script {
    Inline { text: String, module: bool, element: Handle },
    /// `src` as written; resolution against the document base happens later.
    External { href: String, module: bool, element: Handle },
}

/// Every script the document asks for, inline and external, IN DOCUMENT ORDER.
///
/// Order matters: an external bundle usually defines what a later inline
/// script calls, so running inline-only (as the first version did) both misses
/// the content SPAs generate and manufactures failures in scripts whose
/// dependencies never loaded.
pub fn scripts_in_order(d: &Dom) -> Vec<Script> {
    fn go(d: &Dom, h: Handle, out: &mut Vec<Script>) {
        if d.tag(h).map(|t| t.eq_ignore_ascii_case("script")).unwrap_or(false) {
            let ty_ok = match d.attr(h, "type") {
                None => true,
                Some(t) => {
                    let t = t.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                    matches!(t.as_str(), "" | "text/javascript" | "application/javascript"
                        | "text/ecmascript" | "application/ecmascript" | "module")
                }
            };
            if ty_ok {
                let module = d.attr(h, "type")
                    .map(|t| t.trim().eq_ignore_ascii_case("module"))
                    .unwrap_or(false);
                if let Some(src) = d.attr(h, "src") {
                    if !src.trim().is_empty() {
                        out.push(Script::External { href: src.to_string(), module, element: h });
                    }
                } else {
                    let t = d.text_content(h);
                    if !t.trim().is_empty() {
                        out.push(Script::Inline { text: t, module, element: h });
                    }
                }
            }
            return;
        }
        for &c in &d.nodes[h as usize].children { go(d, c, out); }
    }
    let mut out = vec![];
    go(d, d.root(), &mut out);
    out
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

// --- class and style attribute helpers -------------------------------------
//
// Kept in the DOM rather than the engine binding so they can be tested
// directly, and so a second engine gets them for free.

impl Dom {
    pub fn class_list(&self, h: Handle) -> Vec<String> {
        self.attr(h, "class")
            .map(|c| c.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub fn class_add(&mut self, h: Handle, names: &[String]) {
        let mut cur = self.class_list(h);
        for n in names {
            if !n.is_empty() && !cur.iter().any(|c| c == n) { cur.push(n.clone()) }
        }
        self.set_attr(h, "class", &cur.join(" "));
    }

    pub fn class_remove(&mut self, h: Handle, names: &[String]) {
        let cur: Vec<String> = self.class_list(h).into_iter()
            .filter(|c| !names.iter().any(|n| n == c)).collect();
        self.set_attr(h, "class", &cur.join(" "));
    }

    /// Returns the state after toggling.
    pub fn class_toggle(&mut self, h: Handle, name: &str, force: Option<bool>) -> bool {
        let has = self.class_list(h).iter().any(|c| c == name);
        let want = force.unwrap_or(!has);
        if want { self.class_add(h, &[name.to_string()]) }
        else { self.class_remove(h, &[name.to_string()]) }
        want
    }

    /// `style` attribute as declarations, in source order.
    pub fn style_decls(&self, h: Handle) -> Vec<(String, String)> {
        self.attr(h, "style").map(|s| s.split(';').filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() || v.is_empty() { None } else { Some((k.to_ascii_lowercase(), v.to_string())) }
        }).collect()).unwrap_or_default()
    }

    pub fn style_get(&self, h: Handle, prop: &str) -> String {
        let p = css_name(prop);
        self.style_decls(h).into_iter()
            .find(|(k, _)| *k == p).map(|(_, v)| v).unwrap_or_default()
    }

    pub fn style_set(&mut self, h: Handle, prop: &str, value: &str) {
        let p = css_name(prop);
        let mut decls = self.style_decls(h);
        if value.is_empty() {
            decls.retain(|(k, _)| *k != p);
        } else if let Some(d) = decls.iter_mut().find(|(k, _)| *k == p) {
            d.1 = value.to_string();
        } else {
            decls.push((p, value.to_string()));
        }
        let text = decls.iter().map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>().join("; ");
        self.set_attr(h, "style", &text);
    }
}

/// `backgroundColor` -> `background-color`. Scripts use both spellings, and
/// the attribute only ever holds the hyphenated one.
pub fn css_name(prop: &str) -> String {
    if prop.contains('-') { return prop.to_ascii_lowercase() }
    let mut out = String::new();
    for c in prop.chars() {
        if c.is_ascii_uppercase() { out.push('-'); out.push(c.to_ascii_lowercase()) }
        else { out.push(c) }
    }
    out
}
