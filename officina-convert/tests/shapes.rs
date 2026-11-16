//! Drawn shapes and text boxes through .docx and .odt: outline, fill, line
//! with dashes and arrowheads, turn and flips, text and its place, groups.

use officina_convert::{docx, odt};
use officina_text::model::{ArrowEnd, CharAttrs, Dash, Float, Geom, HAlign, ObjectRef, Para, PathCmd, Rel, ShapeData, ShapeKid, ShapeLine, ShapePath, TextAnchor, TextDoc, Wrap, OBJECT_CHAR};

pub fn shapes_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let mut call = ShapeData::new(Geom::Preset { name: "wedgeRectCallout".into(), adj: vec![("adj1".into(), -20_833), ("adj2".into(), 62_500)] });
    call.fill = Some(0xFFC000);
    call.line = Some(ShapeLine { color: 0x1F3864, width: 25, dash: Dash::DashDot, head: ArrowEnd::None, tail: ArrowEnd::None });
    call.rot = 30 * 60_000;
    call.anchor = TextAnchor::Bottom;
    call.inset = [100, 60, 100, 40];
    call.paras.push(Para::new(d.new_id(), "A callout's words", 0));
    let mut arrow = ShapeData::new(Geom::preset("straightConnector1"));
    arrow.line = Some(ShapeLine { color: 0xC00000, width: 30, dash: Dash::Solid, head: ArrowEnd::Oval, tail: ArrowEnd::Triangle });
    arrow.flip_v = true;
    let mut blob = ShapeData::new(Geom::Custom(vec![ShapePath { w: 100, h: 50, cmds: vec![PathCmd::Move(0, 0), PathCmd::Line(100, 0), PathCmd::Cubic(100, 20, 50, 50, 0, 50), PathCmd::Close], filled: true, stroked: true }]));
    blob.fill = Some(0x70AD47);
    blob.line = Some(ShapeLine { color: 0, width: 10, dash: Dash::Dot, head: ArrowEnd::None, tail: ArrowEnd::None });
    let mut group = ShapeData::new(Geom::preset("rect"));
    let mut a = ShapeData::new(Geom::preset("ellipse"));
    a.fill = Some(0x4472C4);
    a.anchor = TextAnchor::Middle;
    a.paras.push(Para::new(d.new_id(), "In a group", 0));
    let mut b = ShapeData::new(Geom::preset("rightArrow"));
    b.fill = Some(0xED7D31);
    b.flip_h = true;
    group.kids = vec![ShapeKid { x: 0, y: 0, w: 50_000, h: 100_000, shape: a }, ShapeKid { x: 60_000, y: 25_000, w: 40_000, h: 50_000, shape: b }];
    let float = |x: i32, y: i32| Some(Float { wrap: Wrap::Square, h_rel: Rel::Margin, h_align: HAlign::Offset, x, v_rel: Rel::Text, y, gap: 120 });
    let body = "Text goes round the shape this paragraph anchors. ".repeat(4);
    for (sd, w, h, f) in [(call, 2880, 1440, float(400, 200)), (arrow, 2400, 600, float(3600, 100)), (blob, 1440, 720, None), (group, 4000, 1600, float(200, 2000))] {
        let id = d.shapes.add(sd).unwrap();
        let at = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: id, width: w, height: h, float: f, shape: true }), ..Default::default() });
        let text = format!("{OBJECT_CHAR}{body}");
        d.push_para(&text, 0, vec![(0, at), (OBJECT_CHAR.len_utf8() as u32, 0)]);
    }
    d
}

/// What should survive a file: per shape, its object's frame and float,
/// outline kind, fill, line, turn, flips, text and its place, and its group.
fn summary(d: &TextDoc) -> Vec<String> {
    let mut out = Vec::new();
    for b in d.blocks.iter() {
        let Some(p) = b.as_para() else { continue };
        for &(_, a) in &p.runs {
            let Some(o) = d.attrs.get(a).object.filter(|o| o.shape) else { continue };
            let sd = d.shapes.get(o.image).unwrap();
            let mut s = format!("{}x{} {:?}", o.width, o.height, o.float.map(|f| (f.x, f.y, f.wrap)));
            fn one(sd: &ShapeData, s: &mut String) {
                let geom = match &sd.geom {
                    Geom::Preset { name, adj } => format!("{name}{:?}", adj.iter().map(|a| a.1).collect::<Vec<_>>()),
                    Geom::Custom(p) => format!("custom {}", p.len()),
                };
                let text: Vec<&str> = sd.paras.iter().map(|p| p.text.as_str()).collect();
                s.push_str(&format!(" [{geom} fill {:?} line {:?} rot {} flip {} {} text {text:?} {:?} {:?}", sd.fill, sd.line, sd.rot, sd.flip_h, sd.flip_v, sd.anchor, sd.inset));
                for k in &sd.kids {
                    s.push_str(&format!(" kid {} {} {} {}", k.x / 100, k.y / 100, k.w / 100, k.h / 100));
                    one(&k.shape, s);
                }
                s.push(']');
            }
            one(sd, &mut s);
            out.push(s);
        }
    }
    out
}

#[test]
fn shapes_survive_docx_and_odt() {
    let d = shapes_doc();
    let want = summary(&d);
    assert_eq!(want.len(), 4);
    let v = docx::write::write(&d, Vec::new()).unwrap();
    let (back, _) = docx::read(&v).unwrap();
    assert_eq!(summary(&back), want, "docx");
    let v = odt::write::write(&d, Vec::new()).unwrap();
    let (back, _) = odt::read::read(&v).unwrap();
    assert_eq!(summary(&back), want, "odt");
    assert!(check(&back).is_empty(), "{:?}", check(&back));
}

/// What a file the shapes went through (LibreOffice's, say) must keep, with
/// room for its own way of writing them: what's wrong, if anything.
pub fn check(d: &TextDoc) -> Vec<String> {
    let mut bad = Vec::new();
    let shapes: Vec<(ObjectRef, &ShapeData)> = d.blocks.iter().filter_map(|b| b.as_para()).flat_map(|p| p.runs.iter().filter_map(|&(_, a)| d.attrs.get(a).object.filter(|o| o.shape))).filter_map(|o| Some((o, &**d.shapes.get(o.image)?))).collect();
    if shapes.len() != 4 {
        bad.push(format!("{} shapes, not 4", shapes.len()));
        return bad;
    }
    let near = |a: i64, b: i64, by: i64| (a - b).abs() <= by;
    let (o, c) = shapes[0];
    if !matches!(&c.geom, Geom::Preset { name, .. } if &**name == "wedgeRectCallout") {
        bad.push(format!("callout outline {:?}", c.geom));
    }
    if c.fill != Some(0xFFC000) || c.line.map(|l| (l.color, l.dash)) != Some((0x1F3864, Dash::DashDot)) {
        bad.push(format!("callout fill {:?} line {:?}", c.fill, c.line));
    }
    if !near(c.rot as i64, 30 * 60_000, 6_000) || c.anchor != TextAnchor::Bottom || c.paras.first().map(|p| p.text.as_str()) != Some("A callout's words") {
        bad.push(format!("callout turn {} text {:?} at {:?}", c.rot, c.paras.iter().map(|p| &p.text).collect::<Vec<_>>(), c.anchor));
    }
    if !near(o.width as i64, 2880, 30) || !near(o.height as i64, 1440, 30) || o.float.is_none_or(|f| !near(f.y as i64, 200, 30)) {
        bad.push(format!("callout frame {}x{} {:?}", o.width, o.height, o.float));
    }
    let (_, a) = shapes[1];
    if a.line.map(|l| (l.color, l.head, l.tail)) != Some((0xC00000, ArrowEnd::Oval, ArrowEnd::Triangle)) || !a.flip_v {
        bad.push(format!("arrow line {:?} flipped {}", a.line, a.flip_v));
    }
    let (_, b) = shapes[2];
    if b.fill != Some(0x70AD47) || b.line.map(|l| l.dash) != Some(Dash::Dot) {
        bad.push(format!("drawn outline fill {:?} line {:?}", b.fill, b.line));
    }
    let (_, g) = shapes[3];
    let texts: Vec<&str> = g.all_paras().iter().map(|p| p.text.as_str()).filter(|t| !t.is_empty()).collect();
    let fills: Vec<Option<u32>> = g.kids.iter().map(|k| k.shape.fill).collect();
    if g.kids.len() != 2 || texts != ["In a group"] || fills != [Some(0x4472C4), Some(0xED7D31)] || !g.kids[1].shape.flip_h {
        bad.push(format!("group of {} text {texts:?} fills {fills:?}", g.kids.len()));
    }
    bad
}
