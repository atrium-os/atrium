//! HTML in, arena DOM out.
//!
//! Tolerance lives HERE and only here. The document profile parses strictly
//! and fails loudly; this is the converter, and the spec (profile §6) puts the
//! twenty-years-of-error-recovery complexity in a component whose output is
//! checkable rather than in the renderer.

use crate::dom::{Dom, Handle, Kind};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle as RcHandle, NodeData, RcDom};

/// Parse an HTML FRAGMENT — the `innerHTML =` path.
///
/// Returned as a Dom whose root children are the fragment's top-level nodes.
/// ★ It goes through the SAME html5ever parse as a document, with the wrapper
/// elements stripped, rather than a second hand-rolled parser: two parsers
/// that disagree about the same bytes is precisely the divergence a converter
/// cannot afford, and it is the bug class this crate exists to avoid.
pub fn parse_fragment(html: &str) -> Dom {
    let doc = parse(&format!("<html><body>{html}</body></html>"));
    // Lift the parsed body's children to the returned root.
    let mut out = Dom::new();
    let root = out.root();
    if let Some(body) = doc.by_tag("body").first().copied() {
        for c in doc.children_of(body) {
            let g = out.graft(&doc, c);
            out.append(root, g);
        }
    }
    out
}

pub fn parse(html: &str) -> Dom {
    let rc = html5ever::parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap_or_else(|_| {
            html5ever::parse_document(RcDom::default(), Default::default()).one("")
        });
    let mut dom = Dom::new();
    let root = dom.root();
    for child in rc.document.children.borrow().iter() {
        convert(child, &mut dom, root);
    }
    dom
}

fn convert(rc: &RcHandle, dom: &mut Dom, parent: Handle) {
    let kind = match &rc.data {
        NodeData::Element { name, .. } => Kind::Element(name.local.to_string()),
        NodeData::Text { contents } => Kind::Text(contents.borrow().to_string()),
        NodeData::Comment { contents } => Kind::Comment(contents.to_string()),
        _ => {
            for c in rc.children.borrow().iter() { convert(c, dom, parent); }
            return;
        }
    };
    let h = dom.create(kind);
    if let NodeData::Element { attrs, .. } = &rc.data {
        for a in attrs.borrow().iter() {
            dom.set_attr(h, &a.name.local.to_string(), &a.value.to_string());
        }
    }
    dom.append(parent, h);
    for c in rc.children.borrow().iter() { convert(c, dom, h); }
}
