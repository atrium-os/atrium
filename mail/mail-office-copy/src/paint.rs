use super::Error;
use mail_office_copy::{OfficeCopy, PagePixels};
use officina_text::{
    layout::{LayoutCache, PinnedFonts},
    lists::Numbering,
    model::TextDoc,
    paginate::Paginator,
    paint::{paint_page, GlyphRun, Item},
    LU,
};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

#[derive(Default)]
struct Outline(PathBuilder);
impl ttf_parser::OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y)
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y)
    }
    fn quad_to(&mut self, x: f32, y: f32, a: f32, b: f32) {
        self.0.quad_to(x, y, a, b)
    }
    fn curve_to(&mut self, x: f32, y: f32, a: f32, b: f32, c: f32, d: f32) {
        self.0.cubic_to(x, y, a, b, c, d)
    }
    fn close(&mut self) {
        self.0.close()
    }
}

pub(super) fn glyphs(
    canvas: &mut Pixmap,
    fonts: &PinnedFonts,
    run: &GlyphRun,
) -> Result<(), Error> {
    if run.turn.is_some() || run.glyphs.is_empty() {
        return Err("unsupported glyph placement".into());
    }
    let face =
        ttf_parser::Face::parse(fonts.face_bytes(run.face), 0).map_err(|_| "missing glyph face")?;
    let unit = 2.0 / LU as f32;
    let scale = run.size as f32 * unit / f32::from(face.units_per_em());
    let mut paint = Paint::default();
    paint.set_color_rgba8(
        (run.color >> 16) as u8,
        (run.color >> 8) as u8,
        run.color as u8,
        255,
    );
    let mut coverage = vec![false; run.text.len()];
    for glyph in &run.glyphs {
        let range = glyph.text.start as usize..glyph.text.end as usize;
        let text = run.text.get(range.clone()).ok_or("invalid glyph cluster")?;
        coverage[range].fill(true);
        if glyph.id == 0 {
            return Err("missing glyph".into());
        }
        let mut path = Outline::default();
        if face
            .outline_glyph(ttf_parser::GlyphId(glyph.id), &mut path)
            .is_none()
        {
            if !text.is_empty() && text.chars().all(char::is_whitespace) {
                continue;
            }
            return Err("missing glyph outline".into());
        }
        let transform = Transform::from_row(
            scale,
            0.,
            0.,
            -scale,
            (run.x + glyph.x) as f32 * unit,
            (run.y + glyph.dy) as f32 * unit,
        );
        let path = path
            .0
            .finish()
            .and_then(|p| p.transform(transform))
            .ok_or("invalid outline")?;
        let bounds = path.bounds();
        if bounds.left() < 0.
            || bounds.top() < 0.
            || bounds.right() > canvas.width() as f32
            || bounds.bottom() > canvas.height() as f32
        {
            return Err("glyph outside page".into());
        }
        canvas.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if coverage.contains(&false) {
        return Err("unpainted glyph cluster".into());
    }
    Ok(())
}

pub fn pages(doc: &TextDoc, expected: &[String]) -> Result<OfficeCopy, Error> {
    if doc.blocks.len() != expected.len()
        || !expected
            .iter()
            .enumerate()
            .all(|(i, t)| doc.para(i).is_some_and(|p| p.text == *t))
    {
        return Err("incomplete paragraph import".into());
    }
    let fonts = PinnedFonts::new()?;
    let mut cache = LayoutCache::default();
    let mut pager = Paginator::new(doc);
    if !pager.run(doc, &mut cache, &fonts, 65) || pager.pages().len() > 64 {
        return Err("too many pages".into());
    }
    let pages = pager.pages();
    let numbering = Numbering::build(doc);
    let mut result = OfficeCopy { pages: Vec::new() };
    let mut encoded_bytes = 0;
    let mut pixels = 0u64;
    let mut painted = String::new();
    for (i, page) in pages.iter().enumerate() {
        let (w, h) = (
            page.setup.width as f64 / 20.,
            page.setup.height as f64 / 20.,
        );
        let (pw, ph) = ((2. * w).ceil() as u32, (2. * h).ceil() as u32);
        pixels = pixels
            .checked_add(u64::from(pw) * u64::from(ph))
            .ok_or("invalid page size")?;
        if u64::from(pw) * u64::from(ph) > 8_000_000 || pixels > 48_000_000 {
            return Err("page image limit".into());
        }
        let mut canvas = Pixmap::new(pw, ph).ok_or("invalid page size")?;
        canvas.fill(tiny_skia::Color::WHITE);
        for item in paint_page(
            doc,
            pages,
            i,
            pager.width(),
            &mut cache,
            &fonts,
            &numbering,
            &pages.len().to_string(),
        ) {
            match item {
                Item::Glyphs(run) => {
                    glyphs(&mut canvas, &fonts, &run)?;
                    painted.push_str(&run.text);
                }
                _ => return Err("unsupported page item".into()),
            }
        }
        let rgb = canvas
            .data()
            .chunks_exact(4)
            .flat_map(|p| p[..3].iter().copied())
            .collect::<Vec<_>>();
        let samples = miniz_oxide::deflate::compress_to_vec_zlib(&rgb, 1);
        encoded_bytes += samples.len();
        if encoded_bytes > 32 << 20 {
            return Err("output too large".into());
        }
        result.pages.push(PagePixels {
            width_twips: page.setup.width.try_into()?,
            height_twips: page.setup.height.try_into()?,
            samples,
        });
    }
    if painted.chars().filter(|c| !c.is_whitespace()).ne(expected
        .iter()
        .flat_map(|s| s.chars())
        .filter(|c| !c.is_whitespace()))
    {
        return Err("incomplete page text".into());
    }
    Ok(result)
}
