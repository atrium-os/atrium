//! Objects and pages (docs/spec/officina.md §16.173): VML pictures,
//! shapes, text boxes and WordArt read as pictures and shapes, and kept
//! through .docx and .odt.

use officina_convert::{docx, odt, zip::ZipWriter};
use officina_text::model::{Rel, TextDoc, Wrap, OBJECT_CHAR};

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:w10="urn:schemas-microsoft-com:office:word" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

/// A one-pixel PNG.
fn png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x03\x08\x02\0\0\0".to_vec();
    v.extend([0u8; 4]);
    v
}

/// A package whose body is `body`, with `word/media/a.png` as rId7.
pub fn docx_of(body: &str) -> Vec<u8> {
    let mut z = ZipWriter::new(Vec::new());
    z.add("[Content_Types].xml", br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#).unwrap();
    z.add("_rels/.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#).unwrap();
    z.add("word/_rels/document.xml.rels", br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/a.png"/></Relationships>"#).unwrap();
    z.add("word/media/a.png", &png()).unwrap();
    z.add("word/document.xml", format!(r#"<?xml version="1.0"?><w:document {W}><w:body>{body}</w:body></w:document>"#).as_bytes()).unwrap();
    z.finish().unwrap()
}

/// Each object in the text: picture or shape, size, float, and the shape's
/// fill and text.
fn objects(d: &TextDoc) -> Vec<(bool, u32, u32, Option<(Wrap, Rel, i32)>, Option<u32>, String)> {
    let mut out = Vec::new();
    for k in 0..d.blocks.len() {
        let Some(p) = d.para(k) else { continue };
        for (i, ch) in p.text.char_indices() {
            if ch != OBJECT_CHAR {
                continue;
            }
            let Some(o) = d.attrs.get(p.attr_at(i as u32)).object else { continue };
            let (fill, text) = if o.shape { d.shapes.get(o.image).map_or((None, String::new()), |s| (s.fill, s.paras.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("|"))) } else { (None, String::new()) };
            out.push((o.shape, o.width, o.height, o.float.map(|f| (f.wrap, f.h_rel, f.x)), fill, text));
        }
    }
    out
}

#[test]
fn vml_pictures_shapes_text_boxes_and_word_art() {
    let body = r##"<w:p><w:r><w:t>Picture </w:t></w:r><w:r><w:pict><v:shape style="width:72pt;height:36pt"><v:imagedata r:id="rId7"/></v:shape></w:pict></w:r></w:p>
<w:p><w:r><w:pict><v:rect style="position:absolute;margin-left:1in;margin-top:10pt;width:2in;height:1in;z-index:-1;mso-position-horizontal-relative:page" fillcolor="#ff0000" strokecolor="blue" strokeweight="2pt"/></w:pict></w:r><w:r><w:t>Rectangle behind</w:t></w:r></w:p>
<w:p><w:r><w:pict><v:shapetype id="_x0000_t202" o:spt="202"/><v:shape type="#_x0000_t202" style="position:absolute;width:144pt;height:72pt" fillcolor="#ffffcc"><v:textbox inset="0,0,0,0"><w:txbxContent><w:p><w:r><w:t>Inside the box</w:t></w:r></w:p></w:txbxContent></v:textbox><w10:wrap type="square"/></v:shape></w:pict></w:r><w:r><w:t>Text box</w:t></w:r></w:p>
<w:p><w:r><w:pict><v:shape style="width:200pt;height:50pt" fillcolor="silver" stroked="f"><v:textpath string="WordArt here"/></v:shape></w:pict></w:r></w:p>"##;
    let (d, l) = docx::read(&docx_of(body)).unwrap();
    let o = objects(&d);
    assert_eq!(o.len(), 4, "{o:?} {:?}", l.report);
    assert_eq!(o[0], (false, 1440, 720, None, None, String::new()));
    assert_eq!((o[1].0, o[1].1, o[1].2, o[1].3, o[1].4), (true, 2880, 1440, Some((Wrap::Behind, Rel::Page, 1440)), Some(0xFF0000)));
    assert_eq!((o[2].0, o[2].3.map(|f| f.0), o[2].4, o[2].5.as_str()), (true, Some(Wrap::Square), Some(0xFFFFCC), "Inside the box"));
    assert_eq!((o[3].4, o[3].5.as_str()), (Some(0xC0C0C0), "WordArt here"));
    // The text box's text isn't also after its paragraph.
    let all: String = (0..d.blocks.len()).filter_map(|k| d.para(k)).map(|p| p.text.clone()).collect();
    assert!(!all.contains("Inside the box"), "{all}");
    // Through both writers, the objects stay.
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(objects(&r), o, "docx");
    let r = odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(objects(&r).len(), 4, "odt");
}

/// Word's frames: framed paragraphs in a row become one text box that
/// floats from the paragraph after them, where the frame puts it.
#[test]
fn frames_become_text_boxes() {
    let fr = r#"<w:pPr><w:framePr w:w="2880" w:h="720" w:hRule="exact" w:wrap="around" w:hAnchor="page" w:vAnchor="page" w:x="1440" w:y="2160"/></w:pPr>"#;
    let body = format!(r#"<w:p><w:r><w:t>Before</w:t></w:r></w:p><w:p>{fr}<w:r><w:t>Framed one</w:t></w:r></w:p><w:p>{fr}<w:r><w:t>Framed two</w:t></w:r></w:p><w:p><w:r><w:t>After</w:t></w:r></w:p><w:p><w:pPr><w:framePr w:xAlign="center" w:y="100" w:vAnchor="text"/></w:pPr><w:r><w:t>Last</w:t></w:r></w:p>"#);
    let (d, _) = docx::read(&docx_of(&body)).unwrap();
    let texts: Vec<String> = (0..d.blocks.len()).filter_map(|k| d.para(k)).map(|p| p.text.replace(OBJECT_CHAR, "[o]")).collect();
    assert_eq!(texts, ["Before", "[o]After", "[o]"]);
    let o = objects(&d);
    assert_eq!((o[0].1, o[0].2, o[0].3, o[0].5.as_str()), (2880, 720, Some((Wrap::Square, Rel::Page, 1440)), "Framed one|Framed two"));
    assert_eq!(o[1].5, "Last");
    let f = d.attrs.get(d.para(2).unwrap().attr_at(0)).object.unwrap().float.unwrap();
    assert_eq!((f.h_align, f.v_rel, f.y), (officina_text::model::HAlign::Center, Rel::Text, 100));
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(objects(&r), o);
}
