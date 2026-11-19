//! Stock title and content layouts. Rebuild the layout chain from Officina's
//! templates so importer fallbacks cannot silently stand in for source styles.
use super::{check, rebuild_shape, Error};
use officina_slides::{model::*, ops::new_slide};
use officina_text::model::CharAttrs;

fn layout_kind(layout: &Layout, odf: bool) -> &str {
    if odf {
        match layout.name.as_ref() {
            "Office Theme - Title Slide" => "title",
            "Office Theme - Title and Content" => "obj",
            "Office Theme - Blank" => "blank",
            _ => "",
        }
    } else {
        &layout.kind
    }
}

pub(super) fn has_layouts(deck: &mut Deck, odf: bool) -> Result<bool, Error> {
    for i in 0..deck.slide_count() {
        let id = deck.slide(i).map_err(|e| e.0)?.layout;
        let layout = deck.layout(id).ok_or("missing slide layout")?;
        if layout_kind(layout, odf) != "blank" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn body(t: &TextBody) -> Result<TextBody, Error> {
    check(
        t.doc.blocks.len() <= 64 && t.text().len() <= 4096,
        "slide text limit",
    )?;
    for bullet in &t.bullets {
        match bullet {
            Bullet::Char { ch, font, .. } => check(
                !ch.is_control()
                    && font
                        .as_deref()
                        .is_none_or(|f| matches!(f, "Calibri" | "Calibri Light" | "Arial")),
                "unsupported bullet glyph or font",
            )?,
            Bullet::Number { scheme, start } => check(
                matches!(
                    scheme.as_ref(),
                    "arabicPlain"
                        | "arabicPeriod"
                        | "arabicParenR"
                        | "arabicParenBoth"
                        | "alphaLcPeriod"
                        | "alphaLcParenR"
                        | "alphaLcParenBoth"
                        | "alphaUcPeriod"
                        | "alphaUcParenR"
                        | "alphaUcParenBoth"
                        | "romanLcPeriod"
                        | "romanLcParenR"
                        | "romanLcParenBoth"
                        | "romanUcPeriod"
                        | "romanUcParenR"
                        | "romanUcParenBoth"
                ) && (1..=3999).contains(start)
                    && (!scheme.starts_with("roman")
                        || *start + (t.doc.blocks.len() as u32) <= 4000),
                "unsupported list numbering",
            )?,
            Bullet::None => {}
        }
    }
    for i in 0..t.doc.blocks.len() {
        let p = t.doc.para(i).ok_or("unsupported slide text block")?;
        check(
            (p.style as usize) < MAX_LEVELS && !p.text.chars().any(char::is_control),
            "unsupported slide paragraph",
        )?;
        if let Some(Some(list)) = &p.props.list {
            check(
                list.id
                    .checked_sub(1)
                    .and_then(|i| t.bullets.get(i as usize))
                    .is_some(),
                "missing list style",
            )?;
        }
        for &(_, aid) in &p.runs {
            let a = t.doc.attrs.get(aid);
            let supported = CharAttrs {
                bold: a.bold,
                italic: a.italic,
                underline: a.underline,
                strike: a.strike,
                size: a.size,
                color: a.color,
                highlight: a.highlight,
                family: a.family.clone(),
                ..Default::default()
            };
            check(
                *a == supported
                    && a.family
                        .as_deref()
                        .is_none_or(|f| matches!(f, "Calibri" | "Calibri Light"))
                    && a.size.is_none_or(|n| (2..=400).contains(&n)),
                "unsupported slide run style or font",
            )?;
        }
    }
    let mut clean = TextBody::plain("");
    clean.doc = t.doc.clone();
    clean.bullets = t.bullets.clone();
    Ok(clean)
}

pub(super) fn rebuild(source: &mut Deck, odf: bool) -> Result<Deck, Error> {
    let mut clean = Deck::new();
    clean.size = source.size;
    for i in 0..source.slide_count() {
        source.load_slide(i).map_err(|e| e.0)?;
        let s = source.slide_if_loaded(i).ok_or("missing slide")?;
        let layout = source.layout(s.layout).ok_or("missing slide layout")?;
        let kind = layout_kind(layout, odf);
        check(
            matches!(kind, "title" | "obj" | "blank"),
            "unsupported slide layout",
        )?;
        check(
            !s.hidden && s.builds.is_empty() && s.shapes.len() <= 512,
            "hidden slide, build or too many shapes",
        )?;
        let layout = clean.layout_of_kind(kind).ok_or("missing stock layout")?.id;
        let mut slide = new_slide(&mut clean, layout);
        check(
            s.shapes.len() >= slide.shapes.len(),
            "missing slide placeholders",
        )?;
        for (expected, actual) in slide.shapes.iter_mut().zip(&s.shapes) {
            let expected_ph = expected.ph.ok_or("missing stock placeholder")?;
            let actual_ph = actual.ph.ok_or("missing slide placeholder")?;
            let matches = expected_ph == actual_ph
                || odf
                    && (expected_ph.kind == actual_ph.kind
                        || expected_ph.kind == PhKind::CtrTitle && actual_ph.kind == PhKind::Title);
            check(matches, "unexpected or ambiguous slide placeholder")?;
            *expected.text_mut().ok_or("missing stock text frame")? =
                body(actual.text().ok_or("missing slide text frame")?)?;
        }
        for shape in &s.shapes[slide.shapes.len()..] {
            check(shape.ph.is_none(), "unexpected slide placeholder")?;
            slide
                .shapes
                .push(rebuild_shape(source, &mut clean, shape, true)?);
        }
        slide.notes = s.notes.as_ref().map(|t| TextBody::plain(&t.text()));
        slide.transition = s.transition.clone();
        clean.push_slide(slide);
    }
    Ok(clean)
}
