//! ★ MARGIN COLLAPSING, compiled into literal margins (CSS 2.1 §8.3.1).
//!
//! The profile's margins are literal and never collapse (G4: a rule's effect
//! is decidable from a bounded context). Real pages are written for
//! collapsing: two paragraphs with `margin: 1em 0` sit 1em apart in a
//! browser and 2em apart in a literal-margin renderer — on every page. So the
//! PRODUCER does the collapsing, the way it compiles `box-sizing`: it finds
//! each chain of adjoining margins and rewrites them so that literal layout
//! gives the collapsed result.
//!
//! A chain is a maximal run of vertical margins that adjoin: a box's bottom
//! and its next in-flow sibling's top, a box's top and its first child's top
//! when nothing (border, padding, a formatting-context boundary) separates
//! them, the same at the bottom, and both margins of an empty box, through
//! which a chain continues. The collapsed value is the largest positive
//! margin plus the most negative one. The literal form puts that whole value
//! on the OUTERMOST margin of the chain and zeroes the rest: the zeroed ones
//! belonged to descendants whose margins escaped their parent, so the
//! geometry is identical.
//!
//! `em` margins are converted to px with the element's OWN computed font
//! size — an `em` on one box is not an `em` on its neighbour. Percentages
//! and `rem` stay symbolic, inside `calc(max(…) + min(…))`, which the renderer
//! resolves. A chain that cannot be expressed (a `calc()` holding `em`, an
//! unknown font size) is left literal and counted.

use crate::{Priority, Report};
use navigator_dom::{Dom, Handle, Kind};
use std::collections::BTreeMap;

pub(crate) type Resolved = BTreeMap<(Handle, String, String), BTreeMap<String, (Priority, String)>>;

/// Effective values in one media context: the context's own, then the
/// base's, then the UA's.
struct Ctx<'a> {
    resolved: &'a Resolved,
    ua: &'a BTreeMap<Handle, BTreeMap<String, String>>,
    dom: &'a Dom,
    ctx: String,
    font: BTreeMap<Handle, Option<f64>>,
}

impl<'a> Ctx<'a> {
    fn get(&self, h: Handle, p: &str) -> Option<String> {
        self.resolved.get(&(h, String::new(), self.ctx.clone())).and_then(|x| x.get(p)).map(|(_, v)| v.clone())
            .or_else(|| self.resolved.get(&(h, String::new(), String::new())).and_then(|x| x.get(p)).map(|(_, v)| v.clone()))
            .or_else(|| self.ua.get(&h).and_then(|x| x.get(p)).cloned())
            .map(|v| v.trim().to_ascii_lowercase())
    }
    fn is_el(&self, h: Handle) -> bool { matches!(self.dom.get(h).map(|n| &n.kind), Some(Kind::Element(_))) }
    fn display(&self, h: Handle) -> String { self.get(h, "display").unwrap_or_else(|| "inline".into()) }
    fn out_of_flow(&self, h: Handle) -> bool { matches!(self.get(h, "position").as_deref(), Some("absolute") | Some("fixed")) }
    /// A block-level box in flow: its margins take part.
    fn block_level(&self, h: Handle) -> bool {
        self.is_el(h) && matches!(self.display(h).as_str(), "block" | "flex" | "grid" | "table") && !self.out_of_flow(h)
    }
    /// A box that starts a new block formatting context: nothing collapses
    /// through its edges (its own margins still collapse with siblings).
    fn bfc_root(&self, h: Handle) -> bool {
        self.display(h) != "block"
            || self.get(h, "overflow-x").is_some_and(|v| v != "visible")
            || self.get(h, "overflow-y").is_some_and(|v| v != "visible")
            || self.out_of_flow(h)
            || self.dom.element_children(self.dom.root()).first() == Some(&h)
    }
    fn zero(&self, v: Option<String>) -> bool {
        match v { None => true, Some(v) => matches!(v.as_str(), "0" | "0px" | "0%" | "0em" | "0rem" | "auto" | "initial") }
    }
    fn has_border(&self, h: Handle, side: &str) -> bool {
        let style = self.get(h, &format!("border-{side}-style")).unwrap_or_else(|| "none".into());
        style != "none" && style != "hidden" && !self.zero(self.get(h, &format!("border-{side}-width")))
    }
    fn solid_edge(&self, h: Handle, side: &str) -> bool {
        self.has_border(h, side) || !self.zero(self.get(h, &format!("padding-{side}")))
    }
    /// An absolutely positioned box whose vertical position is its STATIC
    /// position: where it would sit in flow.
    fn static_abs(&self, h: Handle) -> bool {
        self.out_of_flow(h) && self.get(h, "top").map_or(true, |v| v == "auto") && self.get(h, "bottom").map_or(true, |v| v == "auto")
    }
    /// Children, in flow order. A block inside an inline (`<span><div>`) is
    /// split out of it and joins this flow, so such an inline is looked
    /// through.
    fn items(&self, g: Handle) -> Vec<Item> {
        let mut out = vec![];
        self.items_into(g, &mut out);
        out
    }
    fn items_into(&self, g: Handle, out: &mut Vec<Item>) {
        for c in self.dom.children_of(g) {
            match self.dom.get(c).map(|n| &n.kind) {
                Some(Kind::Text(t)) => if !t.trim().is_empty() { out.push(Item::Line) },
                Some(Kind::Element(_)) => {
                    let d = self.display(c);
                    if d == "none" { continue }
                    if self.out_of_flow(c) { if self.static_abs(c) { out.push(Item::Abs) } continue }
                    if self.block_level(c) { out.push(Item::Block(c)) }
                    else if d == "inline" && self.dom.children_of(c).into_iter().any(|k| self.block_level(k)) { self.items_into(c, out) }
                    else { out.push(Item::Line) }
                }
                _ => {}
            }
        }
    }
    /// An empty box its chain passes straight through.
    fn empty(&self, h: Handle) -> bool {
        self.display(h) == "block" && !self.bfc_root(h)
            && !self.solid_edge(h, "top") && !self.solid_edge(h, "bottom")
            && self.zero(self.get(h, "height")) && self.zero(self.get(h, "min-height"))
            && self.items(h).iter().all(|i| match i { Item::Block(c) => self.empty(*c), Item::Abs => true, Item::Line => false })
    }
    fn passes_top(&self, h: Handle) -> bool { self.display(h) == "block" && !self.bfc_root(h) && !self.solid_edge(h, "top") }
    fn passes_bottom(&self, h: Handle) -> bool {
        self.display(h) == "block" && !self.bfc_root(h) && !self.solid_edge(h, "bottom")
            && self.get(h, "height").map_or(true, |v| v == "auto") && self.zero(self.get(h, "min-height"))
            && self.get(h, "max-height").map_or(true, |v| v == "none")
    }
    /// The element's computed font size in px, or `None` when it is not
    /// statically knowable.
    fn font_px(&mut self, h: Handle) -> Option<f64> {
        if let Some(v) = self.font.get(&h) { return *v }
        let parent = self.dom.get(h).and_then(|n| n.parent).filter(|p| self.is_el(*p));
        let inherited = match parent { Some(p) => self.font_px(p), None => Some(16.0) };
        let root_px = self.dom.element_children(self.dom.root()).first().copied()
            .and_then(|r| if r == h { None } else { Some(r) }).map_or(Some(16.0), |r| self.font_px(r));
        let v = match self.get(h, "font-size") {
            None => inherited,
            Some(v) if v == "inherit" => inherited,
            Some(v) if v == "initial" => Some(16.0),
            Some(v) => length_px(&v, inherited, root_px),
        };
        self.font.insert(h, v);
        v
    }
    /// A margin as an expression the renderer resolves the same way on any
    /// box: px (from em), %, rem, or 0 for `auto`.
    fn margin(&mut self, h: Handle, side: &str) -> Option<String> {
        let v = self.get(h, &format!("margin-{side}")).unwrap_or_else(|| "0px".into());
        if self.zero(Some(v.clone())) { return Some("0px".into()) }
        if v.ends_with("em") && !v.ends_with("rem") && !v.contains('(') {
            let n: f64 = v.trim_end_matches("em").parse().ok()?;
            return Some(fmt_px(n * self.font_px(h)?));
        }
        if v.contains("em") && v.contains('(') && !v.contains("rem") { return None }
        if v == "inherit" { return None }
        Some(v)
    }
}

fn length_px(v: &str, parent: Option<f64>, root: Option<f64>) -> Option<f64> {
    let num = |s: &str| s.trim().parse::<f64>().ok();
    if let Some(n) = v.strip_suffix("rem") { return Some(num(n)? * root?) }
    if let Some(n) = v.strip_suffix("em") { return Some(num(n)? * parent?) }
    if let Some(n) = v.strip_suffix("px") { return num(n) }
    if let Some(n) = v.strip_suffix('%') { return Some(num(n)? / 100.0 * parent?) }
    None
}

fn fmt_px(n: f64) -> String {
    let r = (n * 1000.0).round() / 1000.0;
    format!("{r}px")
}

/// The collapsed value of a chain: largest positive + most negative.
fn collapsed(exprs: &[String]) -> String {
    let px: Option<Vec<f64>> = exprs.iter().map(|e| e.strip_suffix("px").and_then(|n| n.parse::<f64>().ok())).collect();
    match px {
        Some(ns) => {
            let pos = ns.iter().cloned().fold(0.0f64, f64::max);
            let neg = ns.iter().cloned().fold(0.0f64, f64::min);
            fmt_px(pos + neg)
        }
        None => {
            let list = exprs.join(", ");
            format!("calc(max(0px, {list}) + min(0px, {list}))")
        }
    }
}

/// Margin slots: (element, "top" | "bottom").
type Slot = (Handle, &'static str);

/// A child as the margin walk sees it.
#[derive(Clone, Copy)]
enum Item {
    /// An in-flow block-level box: its margins take part.
    Block(Handle),
    /// A line box (text, inline content): nothing collapses across it.
    Line,
    /// A statically positioned out-of-flow box: transparent to collapsing,
    /// but it is PLACED after the margins before it, so they are pinned.
    Abs,
}

impl<'a> Ctx<'a> {
    /// An empty box that holds a statically positioned box: it has a
    /// position that shows, so the margins before it must stay before it.
    fn holds_static(&self, h: Handle) -> bool {
        self.items(h).iter().any(|i| match i { Item::Abs => true, Item::Block(c) => self.holds_static(*c), Item::Line => false })
    }
    /// Every margin of an EMPTY box: its own two and, since its empty
    /// children collapse through it too, all of theirs.
    fn empty_slots(&self, h: Handle, out: &mut Vec<Slot>) {
        out.push((h, "top"));
        for it in self.items(h) { if let Item::Block(c) = it { self.empty_slots(c, out) } }
        out.push((h, "bottom"));
    }
    /// The margins that adjoin at `h`'s top edge, outermost first.
    fn top_chain(&self, h: Handle, out: &mut Vec<Slot>) {
        if self.empty(h) { self.empty_slots(h, out); return }
        out.push((h, "top"));
        if !self.passes_top(h) { return }
        for it in self.items(h) {
            match it {
                Item::Line => return,
                Item::Abs => {}
                Item::Block(c) => {
                    self.top_chain(c, out);
                    if !self.empty(c) { return }
                }
            }
        }
    }
    /// The margins that adjoin at `h`'s bottom edge, outermost first.
    fn bottom_chain(&self, h: Handle, out: &mut Vec<Slot>) {
        out.push((h, "bottom"));
        if !self.passes_bottom(h) || self.empty(h) { return }
        for it in self.items(h).into_iter().rev() {
            match it {
                Item::Line => return,
                Item::Abs => {}
                Item::Block(c) => {
                    if self.empty(c) { self.empty_slots(c, out); continue }
                    self.bottom_chain(c, out);
                    return;
                }
            }
        }
    }
}

/// Compile collapsing for one media context: `(element, side) -> literal`.
///
/// Two walks. The first finds PINS: a statically positioned box is placed
/// after the margins before it have collapsed (§8.3.1, the "hypothetical"
/// position), so that part of a chain must be literal on the slot before the
/// box. The second realizes every chain, putting only the REST of the
/// collapsed value on the chain's anchor.
fn solve(c: &mut Ctx, containers: &[Handle], unresolved: &mut usize) -> BTreeMap<Slot, String> {
    let mut pins: BTreeMap<Slot, String> = BTreeMap::new();
    walk(c, containers, &mut pins, None, unresolved);
    let mut out: BTreeMap<Slot, String> = BTreeMap::new();
    walk(c, containers, &mut pins, Some(&mut out), unresolved);
    out
}

fn px_of(e: &str) -> Option<f64> { e.strip_suffix("px").and_then(|n| n.parse().ok()) }

fn walk(c: &mut Ctx, containers: &[Handle], pins: &mut BTreeMap<Slot, String>, mut out: Option<&mut BTreeMap<Slot, String>>, unresolved: &mut usize) {
    let collecting = out.is_none();
    let mut scratch = BTreeMap::new();
    let out: &mut BTreeMap<Slot, String> = match out.as_deref_mut() { Some(o) => o, None => &mut scratch };
    for &g in containers {
        let d = c.display(g);
        // Only a BLOCK CONTAINER's children collapse with each other: not a
        // flex or grid container's items, not a table's rows.
        if !matches!(d.as_str(), "block" | "inline-block" | "table-cell") { continue }
        let items = c.items(g);
        let n = items.len();
        // Walk the children, building each chain between two breaks.
        let mut i = 0;
        let mut chain: Vec<Slot> = vec![];
        let mut at_start = true; // the chain touches g's top edge
        // A chain that touches g's top edge belongs to g's own top chain when
        // g passes its top through: g's parent realises it.
        let skip_start = c.passes_top(g);
        let skip_end = c.passes_bottom(g);
        let realize = |chain: &mut Vec<Slot>, anchor: Option<Slot>, c: &mut Ctx, out: &mut BTreeMap<Slot, String>, pins: &BTreeMap<Slot, String>, unresolved: &mut usize| {
            if chain.is_empty() || collecting { chain.clear(); return }
            let exprs: Option<Vec<String>> = chain.iter().map(|(h, s)| c.margin(*h, s)).collect();
            let Some(ex) = exprs else { *unresolved += 1; chain.clear(); return };
            let total = collapsed(&ex);
            let pinned: Vec<(Slot, String)> = chain.iter().filter_map(|s| pins.get(s).map(|v| (*s, v.clone()))).collect();
            let mut anchor = anchor.unwrap_or(chain[0]);
            let mut value = total.clone();
            if !pinned.is_empty() {
                // The pinned part is already paid; the anchor carries the rest.
                if pinned.iter().any(|(s, _)| *s == anchor) {
                    match chain.iter().rev().find(|s| !pins.contains_key(*s)) { Some(s) => anchor = *s, None => anchor = (Handle::MAX, "none") }
                }
                let paid: Option<f64> = pinned.iter().map(|(_, v)| px_of(v)).sum();
                value = match (px_of(&total), paid) {
                    (Some(t), Some(p)) => fmt_px(t - p),
                    (None, Some(p)) => format!("{} - {})", total.strip_suffix(')').unwrap_or(&total), fmt_px(p)),
                    _ => { *unresolved += 1; chain.clear(); return }
                };
            }
            for s in chain.iter() {
                let v = if let Some(p) = pins.get(s) { p.clone() } else if *s == anchor { value.clone() } else { "0px".into() };
                out.insert(*s, v);
            }
            chain.clear();
        };
        while i < n {
            match items[i] {
                Item::Abs => {
                    // Placed after the collapsed margins so far: pin them on
                    // the chain's first slot (once per chain).
                    if collecting && !chain.is_empty() && !(at_start && skip_start) && !chain.iter().any(|s| pins.contains_key(s)) {
                        if let Some(ex) = chain.iter().map(|(h, s)| c.margin(*h, s)).collect::<Option<Vec<String>>>() {
                            let a = collapsed(&ex);
                            if px_of(&a).is_some() { pins.insert(chain[0], a); }
                        }
                    }
                }
                Item::Line => {
                    // A line box: whatever chain was open ends at this line.
                    if !(at_start && skip_start) {
                        let anchor = chain.first().copied();
                        realize(&mut chain, anchor, c, out, pins, unresolved);
                    }
                    chain.clear();
                    at_start = false;
                }
                Item::Block(b) => {
                    if c.empty(b) {
                        // §8.3.1: an empty box's top border edge is where it
                        // would be with a bottom border — after the margins
                        // before it AND its own top margin. What it holds is
                        // drawn there, so that much of the chain is pinned.
                        if collecting && c.holds_static(b) && !(at_start && skip_start) && !chain.iter().any(|s| pins.contains_key(s)) {
                            let mut pre = chain.clone();
                            pre.push((b, "top"));
                            if let Some(ex) = pre.iter().map(|(h, s)| c.margin(*h, s)).collect::<Option<Vec<String>>>() {
                                let a = collapsed(&ex);
                                if px_of(&a).is_some() { pins.insert(pre[0], a); }
                            }
                        }
                        c.empty_slots(b, &mut chain);
                    } else {
                        // This box's top chain closes the open chain: the
                        // whole gap sits on its OWN top margin, the
                        // outermost one below the break.
                        let mut top = vec![];
                        c.top_chain(b, &mut top);
                        chain.extend(top);
                        if !(at_start && skip_start) { realize(&mut chain, Some((b, "top")), c, out, pins, unresolved) }
                        chain.clear();
                        at_start = false;
                        let mut bottom = vec![];
                        c.bottom_chain(b, &mut bottom);
                        chain.extend(bottom);
                    }
                }
            }
            i += 1;
        }
        // What is left touches g's bottom edge.
        if !(skip_end && !chain.is_empty()) && !(at_start && skip_start) {
            let anchor = chain.first().copied();
            realize(&mut chain, anchor, c, out, pins, unresolved);
        }
    }
}

/// Rewrite `resolved`'s vertical margins so literal layout equals CSS's
/// collapsed layout, in the base context and in every media context that
/// changes anything the chains depend on.
pub(crate) fn compile(resolved: &mut Resolved, dom: &Dom, ua: &BTreeMap<Handle, BTreeMap<String, String>>, report: &mut Report) {
    const DEPENDS: [&str; 14] = ["margin-top", "margin-bottom", "padding-top", "padding-bottom", "border-top-width", "border-bottom-width",
        "border-top-style", "border-bottom-style", "display", "position", "overflow-x", "overflow-y", "height", "font-size"];
    let mut contexts: Vec<String> = vec![String::new()];
    for ((_, st, media), props) in resolved.iter() {
        if st.is_empty() && !media.is_empty() && props.keys().any(|k| DEPENDS.contains(&k.as_str())) && !contexts.contains(media) {
            contexts.push(media.clone());
        }
    }
    let containers: Vec<Handle> = (0..dom.nodes.len() as Handle).filter(|h| matches!(dom.get(*h).map(|n| &n.kind), Some(Kind::Element(_)))).collect();
    let mut results: Vec<(String, BTreeMap<Slot, String>)> = vec![];
    let mut unresolved = 0usize;
    for ctx in &contexts {
        let mut c = Ctx { resolved, ua, dom, ctx: ctx.clone(), font: BTreeMap::new() };
        let r = solve(&mut c, &containers, &mut unresolved);
        results.push((ctx.clone(), r));
    }
    let base = results[0].1.clone();
    let mut changed = 0usize;
    let pri: Priority = (false, usize::MAX, (0, 1, 0), usize::MAX - 1);
    for (ctx, r) in &results {
        for ((h, side), v) in r {
            let prop = format!("margin-{side}");
            let slot = resolved.entry((*h, String::new(), ctx.clone())).or_default();
            // In a media context, write only what differs from the base
            // result — or what the context itself declared, which the
            // chain has now accounted for.
            if !ctx.is_empty() && base.get(&(*h, *side)) == Some(v) && !slot.contains_key(&prop) { continue }
            let keep = slot.get(&prop).map(|(p, _)| *p).unwrap_or(pri);
            if slot.get(&prop).map(|(_, old)| old != v).unwrap_or(true) { changed += 1 }
            slot.insert(prop, (keep, v.clone()));
        }
    }
    if changed > 0 { report.drop_n("margins collapsed into literal margins", changed) }
    if unresolved > 0 { report.drop_n("margin chain left literal (em inside calc, or an unknown font size)", unresolved) }
}
