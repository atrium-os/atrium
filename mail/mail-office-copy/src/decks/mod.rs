//! Rebuild a static deck, then compare its entire package to the source.
//! Officina's editor can recover damaged slides, so importer success alone
//! cannot authorize export. Render only the independently rebuilt model.
use super::{
    package::{same_xml, xml},
    Error,
};
use mail_office_copy::OfficeCopy;
use officina_convert::{odp, pptx, zip::Archive};
use officina_slides::{model::*, ops::new_slide};
use std::collections::BTreeMap;
mod raster;

fn check(ok: bool, message: &'static str) -> Result<(), Error> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn parts(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let archive = Archive::open(bytes)?;
    check(archive.entries.len() <= 320, "too many deck parts")?;
    let mut out = BTreeMap::new();
    let mut total = 0;
    for e in &archive.entries {
        total += e.size;
        check(
            total <= 8 << 20 && !out.contains_key(&e.name),
            "oversized or duplicate deck part",
        )?;
        let data = archive.read(e)?;
        if e.name.ends_with(".xml") || e.name.ends_with(".rels") {
            let doc = xml(&data)?;
            for n in doc.descendants().filter(|n| n.is_element()) {
                check(
                    !matches!(
                        n.tag_name().name(),
                        "timing"
                            | "bldLst"
                            | "animations"
                            | "videoFile"
                            | "audioFile"
                            | "plugin"
                            | "encryption-data"
                            | "sndAc"
                    ) && !n
                        .tag_name()
                        .namespace()
                        .unwrap_or("")
                        .contains(":animation:"),
                    "deck contains animation, encryption or media",
                )?;
                for a in n.attributes() {
                    check(
                        !matches!(
                            (a.name(), a.value()),
                            ("show", "0" | "false")
                                | ("visibility", "hidden")
                                | ("TargetMode", "External")
                        ),
                        "hidden slide or external relationship",
                    )?;
                }
            }
        } else {
            check(
                e.name == "mimetype" || e.name.ends_with(".png"),
                "unsupported embedded file",
            )?;
        }
        out.insert(e.name.clone(), data);
    }
    Ok(out)
}

fn rebuild_shape(source: &Deck, clean: &mut Deck, s: &Shape) -> Result<Shape, Error> {
    let r = s.frame.ok_or("unsupported inherited frame")?;
    check(
        r.w > 0
            && r.h > 0
            && r.x >= 0
            && r.y >= 0
            && r.x.checked_add(r.w).is_some_and(|v| v <= clean.size.0)
            && r.y.checked_add(r.h).is_some_and(|v| v <= clean.size.1),
        "shape outside slide",
    )?;
    let kind = match &s.kind {
        ShapeKind::Geom { fill, text, .. } => {
            let fill = match fill {
                Some(Fill::Solid(c)) => Fill::Solid(Color::rgb(
                    officina_slides::resolve::color(&source.theme, c) >> 8,
                )),
                None | Some(Fill::None) => Fill::None,
                _ => return Err("unsupported slide fill".into()),
            };
            let text = text.as_ref().map(|t| t.text());
            check(
                text.as_ref()
                    .is_none_or(|t| t.len() <= 4096 && !t.chars().any(char::is_control)),
                "unsupported text frame",
            )?;
            ShapeKind::Geom {
                geom: Some(Geom::preset("rect")),
                fill: Some(fill),
                line: Some(Stroke::none()),
                text: text.as_deref().map(TextBody::plain),
            }
        }
        ShapeKind::Picture { media, crop, .. } => {
            check(
                source
                    .media(*media)
                    .is_some_and(|m| m.mime.as_ref() == "image/png"),
                "unsupported slide picture",
            )?;
            let image = source.media_bytes(*media).map_err(|e| e.0)?;
            let media = clean.add_media("image/png", &image);
            ShapeKind::Picture {
                media,
                crop: *crop,
                line: None,
            }
        }
        _ => return Err("unsupported slide object".into()),
    };
    Ok(Shape::new(clean.new_id(), r, kind))
}

pub(super) fn convert(odf: bool, bytes: &[u8]) -> Result<OfficeCopy, Error> {
    let input = parts(bytes)?;
    let (mut source, loaded) = if odf {
        odp::read(bytes)?
    } else {
        pptx::read(bytes)?
    };
    check(
        loaded.report.is_empty(),
        "Officina reported incomplete slide import",
    )?;
    let mut clean = Deck::new();
    clean.size = source.size;
    check(
        (1..=64).contains(&source.slide_count())
            && [clean.size.0, clean.size.1]
                .iter()
                .all(|n| (635..=30_480_000).contains(n)),
        "invalid slide count or dimensions",
    )?;
    let layout = clean
        .layout_of_kind("blank")
        .ok_or("missing blank layout")?
        .id;
    source.load_slide(0).map_err(|e| e.0)?;
    let source_layout = source
        .slide_if_loaded(0)
        .and_then(|s| source.layout(s.layout))
        .ok_or("missing slide layout")?;
    let master = source
        .master(source_layout.master)
        .ok_or("missing slide master")?;
    for shape in master.shapes.iter().filter(|s| s.ph.is_none()) {
        let decoration = rebuild_shape(&source, &mut clean, shape)?;
        clean.masters[0].shapes.push(decoration);
    }
    for shape in source_layout.shapes.iter().filter(|s| s.ph.is_none()) {
        let decoration = rebuild_shape(&source, &mut clean, shape)?;
        clean
            .layouts
            .iter_mut()
            .find(|l| l.id == layout)
            .ok_or("missing layout")?
            .shapes
            .push(decoration);
    }
    for i in 0..source.slide_count() {
        source.load_slide(i).map_err(|e| e.0)?;
        let s = source.slide_if_loaded(i).ok_or("missing slide")?;
        check(
            !s.hidden && s.builds.is_empty() && s.shapes.len() <= 512,
            "hidden slide, build or too many shapes",
        )?;
        let mut slide = new_slide(&mut clean, layout);
        for shape in &s.shapes {
            slide
                .shapes
                .push(rebuild_shape(&source, &mut clean, shape)?);
        }
        slide.notes = s.notes.as_ref().map(|t| TextBody::plain(&t.text()));
        slide.transition = s.transition.clone();
        clean.push_slide(slide);
    }
    let rebuilt = if odf {
        odp::write(&mut clean, Vec::new())?
    } else {
        pptx::write(&mut clean, Vec::new())?
    };
    let expected = parts(&rebuilt)?;
    check(
        input.keys().eq(expected.keys()),
        "unaccounted slide package part",
    )?;
    for (name, data) in input {
        let other = &expected[&name];
        if data != *other
            && (!(name.ends_with(".xml") || name.ends_with(".rels")) || !same_xml(&data, other)?)
        {
            return Err(format!("incomplete or unsupported deck part: {name}").into());
        }
    }
    raster::pages(&mut clean)
}

#[cfg(test)]
mod tests;
