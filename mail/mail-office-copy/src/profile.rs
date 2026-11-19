//! The editor can recover damaged files and skip unfamiliar markup. For
//! export, account for each package part and every visible paragraph first.
use super::Error;
use officina_convert::zip::Archive;
use roxmltree::{Document, Node};
use std::collections::BTreeSet;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const MAIN: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const ODT: &str = "application/vnd.oasis.opendocument.text";
const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const M: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";

fn need(ok: bool) -> Result<(), Error> {
    if ok {
        Ok(())
    } else {
        Err("document uses unsupported structure or formatting".into())
    }
}

fn document(bytes: &[u8]) -> Result<Document<'_>, Error> {
    let opts = roxmltree::ParsingOptions {
        nodes_limit: 100_000,
        ..Default::default()
    };
    Ok(Document::parse_with_options(
        std::str::from_utf8(bytes)?,
        opts,
    )?)
}

fn body(xml: &Document, odt: bool) -> Result<Vec<String>, Error> {
    let mut paragraphs = Vec::new();
    let mut bodies = 0;
    for n in xml.descendants() {
        if n.is_pi() {
            return Err("unsupported XML instruction".into());
        }
        if !n.is_element() {
            continue;
        }
        let tag = n.tag_name();
        let parent = n.parent_element().map(|p| p.tag_name().name());
        let name = tag.name();
        let ns = if odt && name == "p" {
            T
        } else if odt {
            O
        } else {
            W
        };
        let prefix = if ns == T {
            "text:"
        } else if ns == O {
            "office:"
        } else {
            "w:"
        };
        need(
            tag.namespace() == Some(ns)
                && xml.input_text()[n.range()].starts_with(&format!("<{prefix}")),
        )?;
        let valid = if odt {
            matches!(
                (parent, name),
                (None, "document-content")
                    | (Some("document-content"), "body")
                    | (Some("body"), "text")
                    | (Some("text"), "p")
            )
        } else {
            matches!(
                (parent, name),
                (None, "document")
                    | (Some("document"), "body")
                    | (Some("body"), "p")
                    | (Some("p"), "r")
                    | (Some("r"), "t")
                    | (Some("r"), "rPr")
                    | (Some("rPr"), "b" | "i")
                    | (Some("p"), "pPr")
                    | (Some("pPr"), "pageBreakBefore")
            )
        };
        need(valid)?;
        if name == "body" {
            bodies += 1;
        }
        for a in n.attributes() {
            need(
                (odt && name == "document-content"
                    && a.namespace() == Some(O)
                    && a.name() == "version"
                    && matches!(a.value(), "1.2" | "1.3"))
                    || (!odt
                        && name == "t"
                        && a.namespace() == Some(roxmltree::NS_XML_URI)
                        && a.name() == "space"
                        && a.value() == "preserve"),
            )?;
        }
        if name == "p" {
            paragraphs.push(String::new());
        }
        for child in n.children().filter(Node::is_text) {
            let text = child.text().unwrap_or_default();
            if (odt && name == "p") || (!odt && name == "t") {
                need(text.chars().all(|c| !c.is_control()))?;
                paragraphs
                    .last_mut()
                    .ok_or("text outside paragraph")?
                    .push_str(text);
            } else {
                need(text.trim().is_empty())?;
            }
        }
    }
    need(bodies == 1 && !paragraphs.is_empty() && paragraphs.len() <= 4096)?;
    Ok(paragraphs)
}

pub fn check(odt: bool, bytes: &[u8]) -> Result<Vec<String>, Error> {
    let archive = Archive::open(bytes)?;
    let mut names = BTreeSet::new();
    let mut size = 0;
    for entry in &archive.entries {
        size += entry.size;
        need(names.insert(entry.name.as_str()) && size <= 8 << 20)?;
    }
    let required = if odt {
        ["mimetype", "META-INF/manifest.xml", "content.xml"]
    } else {
        ["[Content_Types].xml", "_rels/.rels", "word/document.xml"]
    };
    if names != required.into_iter().collect() {
        return officina_plain_package(&archive, odt, required[2]);
    }
    let part = |name: &str| -> Result<Vec<u8>, Error> {
        Ok(archive.read_name(name)?.ok_or("missing package part")?)
    };
    if odt {
        need(part("mimetype")? == ODT.as_bytes())?;
        let raw = part(required[1])?;
        let doc = document(&raw)?;
        let root = doc.root_element();
        need(root.has_tag_name((M, "manifest")))?;
        let mut paths = BTreeSet::new();
        for e in root.descendants().filter(Node::is_element).skip(1) {
            need(e.parent_element() == Some(root) && e.has_tag_name((M, "file-entry")))?;
            let path = e.attribute((M, "full-path")).unwrap_or("");
            let ty = e.attribute((M, "media-type")).unwrap_or("");
            need(
                paths.insert(path)
                    && ((path == "/" && ty == ODT) || (path == "content.xml" && ty == "text/xml")),
            )?;
        }
        need(paths.len() == 2)?;
    } else {
        let raw = part(required[0])?;
        let doc = document(&raw)?;
        let root = doc.root_element();
        need(root.has_tag_name((CT, "Types")))?;
        let mut mains = 0;
        for e in root.descendants().filter(Node::is_element).skip(1) {
            need(e.parent_element() == Some(root))?;
            if e.has_tag_name((CT, "Override")) {
                need(
                    e.attribute("PartName") == Some("/word/document.xml")
                        && e.attribute("ContentType") == Some(MAIN),
                )?;
                mains += 1;
            } else {
                need(
                    e.has_tag_name((CT, "Default"))
                        && matches!(
                            (e.attribute("Extension"), e.attribute("ContentType")),
                            (
                                Some("rels"),
                                Some("application/vnd.openxmlformats-package.relationships+xml")
                            ) | (Some("xml"), Some("application/xml"))
                        ),
                )?;
            }
        }
        need(mains == 1)?;
        let raw = part(required[1])?;
        let rel = document(&raw)?;
        let root = rel.root_element();
        let children: Vec<_> = root
            .descendants()
            .filter(Node::is_element)
            .skip(1)
            .collect();
        need(root.has_tag_name((REL, "Relationships")) && children.len() == 1)?;
        let e = children[0];
        need(e.has_tag_name((REL,"Relationship")) && e.attribute("Target")==Some("word/document.xml") && e.attribute("TargetMode").is_none() &&
            e.attribute("Id").is_some_and(|s| !s.is_empty()) && e.attribute("Type")==Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"))?;
    }
    body(&document(&part(required[2])?)?, odt)
}

/// Officina includes its default stylesheet and page setup even in an
/// unformatted letter. Recognise that exact package by reconstructing it
/// solely from its paragraphs. Every uncompressed part must agree, including
/// relationships and styles. No source XML or opaque extensions are copied.
fn officina_plain_package(archive: &Archive, odt: bool, main: &str) -> Result<Vec<String>, Error> {
    use officina_text::model::TextDoc;
    need(archive.entries.len() <= 12)?;
    let raw = archive.read_name(main)?.ok_or("missing document body")?;
    let xml = document(&raw)?;
    let body_tag = if odt { (O, "text") } else { (W, "body") };
    let body = xml
        .descendants()
        .find(|n| n.has_tag_name(body_tag))
        .ok_or("missing body")?;
    let mut doc = TextDoc::new();
    doc.blocks.remove(0);
    let mut expected = Vec::new();
    for p in body
        .children()
        .filter(|p| p.has_tag_name((if odt { T } else { W }, "p")))
    {
        need(expected.len() < 4096)?;
        let text: String = p
            .descendants()
            .filter(Node::is_text)
            .filter_map(|n| n.text())
            .collect();
        doc.push_para(&text, 0, Vec::new());
        expected.push(text);
    }
    need(!expected.is_empty())?;
    let canonical = if odt {
        officina_convert::odt::write(&doc, Vec::new())?
    } else {
        officina_convert::docx::write(&doc, Vec::new())?
    };
    let clean = Archive::open(&canonical)?;
    need(clean.entries.len() == archive.entries.len())?;
    for e in &clean.entries {
        let original = archive
            .read_name(&e.name)?
            .ok_or("unsupported package part")?;
        need(original == clean.read(e)?)?;
    }
    Ok(expected)
}
