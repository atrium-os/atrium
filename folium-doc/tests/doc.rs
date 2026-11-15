//! The document layer over a hand-built file: inherited attributes, a
//! loop in the page tree, outline with named and explicit destinations,
//! page labels.

use folium_doc::{Doc, Fit, Target};
use folium_syntax::{Bytes, Document};

fn build(objs: &[&str]) -> Vec<u8> {
    let mut f = b"%PDF-1.7\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(f.len());
        f.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let x = f.len();
    f.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    f.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R /Info 9 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    f
}

fn doc() -> Doc {
    let f = build(&[
        // 1 catalog
        "<< /Type /Catalog /Pages 2 0 R /Outlines 6 0 R /PageLabels << /Nums [0 << /S /r >> 2 << /S /D /P (A-) /St 5 >>] >> /Names << /Dests 10 0 R >> >>",
        // 2 root: Letter, rotate 90; kids: page 3, node 4 (which lists node 2 again: a loop)
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 3 /MediaBox [0 0 612 792] /Rotate 90 /Resources << /Font << >> >> >>",
        "<< /Type /Page /Parent 2 0 R >>",
        "<< /Type /Pages /Parent 2 0 R /Kids [5 0 R 2 0 R 11 0 R] /Count 2 /MediaBox [0 0 200 100] /Rotate 0 >>",
        "<< /Type /Page /Parent 4 0 R /CropBox [-10 -10 100 50] /UserUnit 2 >>",
        // 6 outline: two items, the second with a child
        "<< /Type /Outlines /First 7 0 R /Count 3 >>",
        "<< /Title (Intro) /Next 8 0 R /Dest [3 0 R /Fit] >>",
        "<< /Title <FEFF00500061007200740020004200A0> /First 12 0 R /Count 1 /A << /S /GoTo /D (chap2) >> >>",
        "<< /Title (Doc title) /Producer (hand) >>",
        "<< /Names [(chap2) [5 0 R /XYZ 10 20 null]] /Limits [(chap2) (chap2)] >>",
        "<< /Type /Page /Parent 4 0 R >>",
        "<< /Title (Web) /A << /S /URI /URI (https://example.org/) >> >>",
    ]);
    Doc::from_syntax(Document::open(Box::new(Bytes(f.into()))).unwrap()).unwrap()
}

#[test]
fn pages_and_inheritance() {
    let d = doc();
    assert_eq!(d.page_count(), 3);
    assert!(d.tree_damaged, "the loop back to the root is noticed");
    let p0 = d.page(0).unwrap();
    assert_eq!(p0.rotate, 90);
    assert_eq!(p0.size(), (792.0, 612.0));
    assert!(p0.resources.get("Font").is_some());
    let p1 = d.page(1).unwrap();
    assert_eq!(p1.rotate, 0);
    // Crop box cut to the media box, then the user unit.
    assert_eq!(p1.size(), (200.0, 100.0));
    assert_eq!(d.page(2).unwrap().media_box.width(), 200.0);
    assert!(d.page(3).is_err());
}

#[test]
fn outline_and_dests() {
    let d = doc();
    let o = d.outline();
    assert_eq!(o.len(), 2);
    assert_eq!(o[0].title, "Intro");
    assert_eq!(o[0].target, Some(Target::Page { index: 0, fit: Fit::Fit }));
    assert_eq!(o[1].title, "Part B\u{A0}");
    assert_eq!(o[1].target, Some(Target::Page { index: 1, fit: Fit::Xyz { left: Some(10.0), top: Some(20.0), zoom: None } }));
    assert!(o[1].open);
    assert_eq!(o[1].children[0].target, Some(Target::Uri("https://example.org/".into())));
}

#[test]
fn labels_and_info() {
    let d = doc();
    assert_eq!(d.page_labels().unwrap(), vec!["i", "ii", "A-5"]);
    let i = d.info();
    assert_eq!(i.title.as_deref(), Some("Doc title"));
    assert_eq!(i.producer.as_deref(), Some("hand"));
}
