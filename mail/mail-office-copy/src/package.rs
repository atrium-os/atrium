use super::Error;
use roxmltree::{Document, Node};
const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
pub(super) fn xml(b: &[u8]) -> Result<Document<'_>, Error> {
    Ok(Document::parse_with_options(
        std::str::from_utf8(b)?,
        roxmltree::ParsingOptions {
            nodes_limit: 120_000,
            ..Default::default()
        },
    )?)
}
/// Compare XML by expanded names, attribute values and text, ignoring only
/// serialization differences. Keep significant whitespace in leaf elements.
pub(super) fn same_xml(a: &[u8], b: &[u8]) -> Result<bool, Error> {
    fn same(a: Node, b: Node, depth: usize) -> bool {
        if depth > 80 || a.node_type() != b.node_type() {
            return false;
        }
        if !a.is_element() && !a.is_root() {
            return a.text() == b.text() && a.pi() == b.pi();
        }
        if a.is_element()
            && b.namespaces()
                .any(|ns| a.lookup_namespace_uri(ns.name()) != Some(ns.uri()))
        {
            return false;
        }
        if a.tag_name() != b.tag_name()
            || a.attributes().len() != b.attributes().len()
            || !a.attributes().all(|x| {
                b.attributes().any(|y| {
                    y.name() == x.name() && y.namespace() == x.namespace() && y.value() == x.value()
                })
            })
        {
            return false;
        }
        let mixed = |n: Node| n.has_tag_name((TEXT, "p")) || n.has_tag_name((TEXT, "span"));
        let keep = |n: &Node| {
            !(n.is_text()
                && n.text().unwrap_or("").trim().is_empty()
                && n.parent()
                    .is_some_and(|p| !mixed(p) && p.children().any(|c| c.is_element())))
        };
        let mut ac = a.children().filter(keep);
        let mut bc = b.children().filter(keep);
        loop {
            match (ac.next(), bc.next()) {
                (Some(x), Some(y)) if same(x, y, depth + 1) => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    }
    Ok(same(xml(a)?.root(), xml(b)?.root(), 0))
}
