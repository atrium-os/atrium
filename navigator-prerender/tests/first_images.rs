//! The image-free first display (legacy-web §5.4.5.7): the first artifact
//! waits for the document's stylesheets and not for its images, which it
//! names as `pending` with the box they will have; the images artifact
//! follows, bound to that first artifact, and the two merged are — byte for
//! byte — the first artifact a conversion that waited for its images
//! writes. The rest of the progressive conversion is untouched.

use navigator_prerender::{artifact, boa_impl::BoaEngine, convert_progressive, convert_tier1, convert_with,
    fetch::Fetcher, Stage, TierPolicy};
use std::cell::RefCell;
use std::rc::Rc;

const URL: &str = "https://a.example/p.html";

/// A PNG header: enough for its natural size.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

/// One declared image whose real ratio differs, one undeclared, one that
/// fails, one twice, and a stylesheet.
const PAGE: &str = r#"<!doctype html><html><head><link rel=stylesheet href="/s.css"></head><body>
<p id=lead>served</p>
<img src="/declared.png" width="40" height="20">
<img src="/bare.png">
<img src="/missing.png">
<img src="/bare.png">
<script>document.getElementById('lead').textContent = 'served, then scripted';</script>
</body></html>"#;

/// Serves the sheet and the images, logging each request — and every
/// `Stage` the conversion reaches, in the same log, so the order of the two
/// can be checked. `delay` slows every image.
#[derive(Clone, Default)]
struct Site { log: Rc<RefCell<Vec<String>>>, delay: std::time::Duration, no_headers: bool, http_errors: bool }
impl Fetcher for Site {
    fn get(&mut self, url: &str) -> Result<String, String> {
        self.log.borrow_mut().push(url.to_string());
        if url.ends_with(".css") { Ok("p { color: red }".into()) } else { Err(format!("404 {url}")) }
    }
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, String> {
        if !url.ends_with(".png") { return self.get(url).map(String::into_bytes) }
        self.log.borrow_mut().push(url.to_string());
        std::thread::sleep(self.delay);
        Site::image(url)
    }
    /// A header (cut A): logged apart from the bodies, never slowed.
    fn peek(&mut self, url: &str, start: usize, len: usize) -> Result<Vec<u8>, String> {
        self.log.borrow_mut().push(format!("peek {url} {start} {len}"));
        if self.no_headers { return Err("not in time".into()) }
        if self.http_errors && url.ends_with("/missing.png") { return Err(format!("fetch {url}: HTTP 404")) }
        let b = Site::image(url)?;
        Ok(b[start.min(b.len())..(start + len).min(b.len())].to_vec())
    }
}

impl Site {
    fn image(url: &str) -> Result<Vec<u8>, String> {
        if url.ends_with("/declared.png") { Ok(png(400, 300)) }
        else if url.ends_with("/bare.png") { Ok(png(64, 32)) }
        else { Err(format!("404 {url}")) }
    }
}

struct Parts { stages: Vec<Stage>, first: String, images: Vec<String>, readable: String, trans: String, log: Vec<String> }

fn parts(site: Site) -> Parts {
    let p = TierPolicy::default();
    let mut eng = BoaEngine { explore: true, ..Default::default() };
    let mut f = site.clone();
    let (mut stages, mut first, mut images, mut readable, mut trans) = (vec![], String::new(), vec![], String::new(), String::new());
    convert_progressive(PAGE, Some(URL), &mut eng, &mut f, false, &mut |s, c| {
        stages.push(s);
        site.log.borrow_mut().push(format!("{s:?}"));
        match s {
            Stage::First => first = artifact::emit(Some(URL), c, &p),
            Stage::Images { complete } => images.push(artifact::emit_images(Some(URL), c, &first, complete)),
            Stage::Settled => readable = artifact::emit_readable(Some(URL), c, &p),
            Stage::Done => trans = artifact::emit_transitions(Some(URL), c, &p, &readable),
        }
    });
    let log = site.log.borrow().clone();
    Parts { stages, first, images, readable, trans, log }
}

/// Today's first artifact: the tier-1 conversion that waits for its images.
fn waited() -> String {
    let c = convert_tier1(PAGE, Some(URL), &mut Site::default());
    artifact::emit(Some(URL), &c, &TierPolicy::default())
}

#[test]
fn the_first_artifact_fetches_its_sheets_and_no_image() {
    let p = parts(Site::default());
    let at = p.log.iter().position(|l| l == "First").unwrap();
    assert!(p.log[..at].iter().any(|u| u.ends_with("/s.css")), "{:?}", p.log);
    assert!(!p.log[..at].iter().any(|u| u.ends_with(".png")), "an image was fetched before the first display: {:?}", p.log);
    // Only the undeclared images' headers were asked for, each once, from
    // their first byte, 16 KiB at a time.
    let peeks: Vec<&String> = p.log[..at].iter().filter(|u| u.starts_with("peek ")).collect();
    assert_eq!(peeks, vec!["peek https://a.example/bare.png 0 16384", "peek https://a.example/missing.png 0 16384"], "{:?}", p.log);
    assert!(!p.log[at..].iter().any(|u| u.starts_with("peek ")));
    assert_eq!(p.stages, vec![Stage::First, Stage::Images { complete: true }, Stage::Settled, Stage::Done]);
    // Every image is fetched before any script runs the page, and once.
    let images = p.log.iter().position(|l| l.starts_with("Images")).unwrap();
    assert!(p.log[at..images].iter().filter(|u| u.ends_with(".png")).count() == 3, "{:?}", p.log);
    assert_eq!(p.log.iter().filter(|u| u.ends_with("/bare.png")).count(), 1, "{:?}", p.log);
}

#[test]
fn pending_images_say_how_their_box_is_sized() {
    let p = parts(Site::default());
    assert!(p.first.contains(r#"{ "src": "/declared.png", "width": 40, "height": 20, "ratio": "40/20", "address": "", "pending": "declared" }"#), "{}", p.first);
    // Cut A: sized from its header — the size the whole image will have.
    assert!(p.first.contains(r#"{ "src": "/bare.png", "width": 64, "height": 32, "ratio": "64/32", "address": "", "pending": "header" }"#), "{}", p.first);
    // A failed image is not known to have failed yet: it keeps its box too,
    // a placeholder, since its header could not be had.
    assert!(p.first.contains(r#"{ "src": "/missing.png", "width": null, "height": null, "ratio": "none", "address": "", "pending": "placeholder" }"#));
    assert!(!p.first.contains("blake3:"), "the first artifact names no image bytes");
}

#[test]
fn first_merged_with_its_images_is_the_waited_first_artifact_byte_for_byte() {
    let p = parts(Site::default());
    let last = p.images.last().unwrap();
    assert!(artifact::images_complete(last));
    let merged = artifact::merge_images(&p.first, last).unwrap();
    assert_eq!(merged, waited());
    assert!(!merged.contains("\"pending\""));
}

#[test]
fn the_rest_of_the_conversion_is_unchanged() {
    let p = parts(Site::default());
    let mut eng = BoaEngine { explore: true, ..Default::default() };
    let c = convert_with(PAGE, Some(URL), &mut eng, &mut Site::default());
    assert_eq!(artifact::merge(&p.readable, &p.trans).unwrap(), artifact::emit(Some(URL), &c, &TierPolicy::default()));
}

#[test]
fn images_refuse_another_first_artifact() {
    let p = parts(Site::default());
    let other = p.first.replace(">served<", ">served elsewhere<");
    assert!(artifact::merge_images(&other, p.images.last().unwrap()).unwrap_err().contains("another document"));
    assert!(artifact::merge_images(&p.first, &p.trans).is_err());
}

#[test]
fn a_document_without_images_sends_no_images_artifact_and_its_first_is_unchanged() {
    const NONE: &str = "<html><body><p>no pictures</p><img src=\"data:image/png;base64,\"></body></html>";
    let mut eng = BoaEngine::default();
    let (mut stages, mut first) = (vec![], String::new());
    convert_progressive(NONE, None, &mut eng, &mut Site::default(), false, &mut |s, c| {
        stages.push(s);
        if s == Stage::First { first = artifact::emit(None, c, &TierPolicy::default()) }
    });
    assert_eq!(stages, vec![Stage::First, Stage::Settled, Stage::Done]);
    let c = convert_tier1(NONE, None, &mut Site::default());
    assert_eq!(first, artifact::emit(None, &c, &TierPolicy::default()));
}

#[test]
fn slow_images_stream_partial_artifacts_each_the_whole_list_so_far() {
    let p = parts(Site { delay: navigator_prerender::IMAGES_EVERY + std::time::Duration::from_millis(30), ..Default::default() });
    assert!(p.images.len() >= 2, "{} images artifacts", p.images.len());
    let (partial, last) = (&p.images[0], p.images.last().unwrap());
    assert!(!artifact::images_complete(partial) && artifact::images_complete(last));
    // The first partial one: the declared image has arrived, the others
    // are still pending exactly as the first artifact wrote them.
    let m = artifact::merge_images(&p.first, partial).unwrap();
    assert!(m.contains(r#"{ "src": "/declared.png", "width": 400, "height": 300, "ratio": "400/300", "address": "blake3:"#), "{m}");
    assert!(m.contains(r#""pending": "header""#) && m.contains(r#""pending": "placeholder""#));
    assert_eq!(artifact::merge_images(&p.first, last).unwrap(), waited());
}

#[test]
fn an_image_whose_header_proves_it_will_fail_is_left_out_as_the_final_leaves_it() {
    let p = parts(Site { http_errors: true, ..Default::default() });
    // No entry: the normalizer leaves out an `<img>` it has no entry for,
    // as it will once the images are in.
    assert!(!p.first.contains(r#"{ "src": "/missing.png""#), "{}", p.first);
    let last = p.images.last().unwrap();
    assert!(!last.contains(r#"{ "src": "/missing.png""#));
    assert_eq!(artifact::merge_images(&p.first, last).unwrap(), waited());
}

#[test]
fn a_header_not_in_hand_leaves_a_placeholder_and_changes_nothing_else() {
    let p = parts(Site { no_headers: true, ..Default::default() });
    assert!(p.first.contains(r#"{ "src": "/bare.png", "width": null, "height": null, "ratio": "none", "address": "", "pending": "placeholder" }"#));
    assert_eq!(artifact::merge_images(&p.first, p.images.last().unwrap()).unwrap(), waited());
    let q = parts(Site::default());
    assert_eq!((p.readable, p.trans), (q.readable, q.trans));
}

#[test]
fn it_is_reproducible() {
    let (a, b) = (parts(Site::default()), parts(Site::default()));
    assert_eq!((a.first, a.images, a.readable, a.trans), (b.first, b.images, b.readable, b.trans));
}
