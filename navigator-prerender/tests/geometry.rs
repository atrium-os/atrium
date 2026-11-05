//! ★★ LAYOUT GEOMETRY (legacy-web §5.4.5.6e): a script's layout reads are
//! answered from a real layout of the live document, at the conversion
//! viewport (1280 × 600, scroll 0), by the lane's own normalizer and
//! renderer. Each pattern the investigation found in the corpus is pinned:
//! the scroll-spy, the equaliser (whose write is refused), the visibility
//! loader, the observers, and relayout after a mutation within a budget.
//!
//! The documents use plain CSS: the converter's layout normalizes them
//! first, exactly as the lane does.

use navigator_prerender::{boa_impl::BoaEngine, convert, geometry::MAX_LAYOUTS};

fn run(html: &str) -> navigator_prerender::Conversion {
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    c
}

/// Without the reader's sweep: what the page sees at load, at scroll 0.
fn run_unscrolled(html: &str) -> navigator_prerender::Conversion {
    let c = convert(html, &mut BoaEngine { no_scroll_sweep: true, ..Default::default() });
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    c
}

/// The start tag of the element carrying `id="<id>"`.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let at = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no #{id} in {html}"));
    let open = html[..at].rfind('<').unwrap();
    &html[open..open + html[open..].find('>').unwrap()]
}

/// The value of `attr` on the element carrying `id="<id>"`.
fn attr(html: &str, id: &str, attr: &str) -> String {
    let tag = tag(html, id);
    let k = format!(" {attr}=\"");
    let i = tag.find(&k).unwrap_or_else(|| panic!("#{id} has no {attr}: {tag}"));
    tag[i + k.len()..].split('"').next().unwrap().to_string()
}

const BASE: &str = "<style>body{margin:0} h2{margin:0;height:400px;font-size:20px}</style>";

/// ★ The doc13 case (mdbook's scroll-spy): the header to mark is the last
/// one whose top is at or above the scroll line. At scroll 0 that is the
/// FIRST; with every top reported as 0 it was the last.
#[test]
fn a_scroll_spy_marks_the_first_header_at_scroll_zero() {
    let c = run(&format!("<html><head>{BASE}</head><body><h2 id=a>A</h2><h2 id=b>B</h2><h2 id=c>C</h2><script>\
        var cur = null; document.querySelectorAll('h2').forEach(function (h) {{ if (h.offsetTop <= scrollY + 20) cur = h; }});\
        cur.setAttribute('data-active', '1');\
        document.body.setAttribute('data-tops', [].map.call(document.querySelectorAll('h2'), function (h) {{ return h.offsetTop }}).join(','));\
        </script></body></html>"));
    assert_eq!(attr(&c.html, "a", "data-active"), "1");
    assert!(!tag(&c.html, "c").contains("data-active"));
    assert!(c.html.contains(r#"data-tops="0,400,800""#), "{}", c.html);
    assert!(c.geo.layouts >= 1 && c.geo.reads >= 3, "{:?}", c.geo);
}

#[test]
fn rects_are_real_and_an_element_without_a_box_is_zero() {
    let c = run("<html><head><style>body{margin:0} #a{margin-left:30px;width:200px;height:50px} #h{display:none}</style></head>\
        <body><div id=a>x</div><div id=h>hidden</div><p id=out></p><script>\
        var r = document.getElementById('a').getBoundingClientRect();\
        var z = document.getElementById('h').getBoundingClientRect();\
        document.getElementById('out').setAttribute('data-r', [r.left, r.top, r.width, r.height, r.right, r.bottom].join(','));\
        document.getElementById('out').setAttribute('data-z', [z.width, z.height, document.getElementById('h').offsetWidth,\
           document.getElementById('h').getClientRects().length, document.getElementById('a').getClientRects().length].join(','));\
        document.getElementById('out').setAttribute('data-vp', [document.documentElement.clientWidth, document.documentElement.clientHeight].join(','));\
        </script></body></html>");
    assert_eq!(attr(&c.html, "out", "data-r"), "30,0,200,50,230,50");
    assert_eq!(attr(&c.html, "out", "data-z"), "0,0,0,0,1", "display:none has no box, as in a browser");
    assert_eq!(attr(&c.html, "out", "data-vp"), "1280,600");
}

/// An inline element wrapped across lines has a rect per line.
#[test]
fn an_inline_element_has_a_rect_per_line() {
    let c = run("<html><head><style>body{margin:0} div{width:80px}</style></head><body><div>a <span id=s>bbbb cccc dddd eeee</span></div>\
        <p id=out></p><script>var s = document.getElementById('s');\
        document.getElementById('out').setAttribute('data-n', String(s.getClientRects().length));\
        document.getElementById('out').setAttribute('data-cw', String(s.clientWidth));</script></body></html>");
    assert!(attr(&c.html, "out", "data-n").parse::<u32>().unwrap() >= 2, "{}", c.html);
    assert_eq!(attr(&c.html, "out", "data-cw"), "0", "an inline box has no client size");
}

#[test]
fn offset_parent_is_the_positioned_ancestor() {
    let c = run("<html><head><style>body{margin:0} #p{position:relative;margin-top:100px;padding-top:7px;border-top:3px solid #000} #c{height:10px}</style></head>\
        <body><div id=p><div id=c></div></div><p id=out></p><script>var e = document.getElementById('c');\
        document.getElementById('out').setAttribute('data-o', [e.offsetParent.id, e.offsetTop, e.getBoundingClientRect().top].join(','));\
        </script></body></html>");
    // 7 px below the parent's padding edge (border 3, padding 7); 110 in the page.
    assert_eq!(attr(&c.html, "out", "data-o"), "p,7,110");
}

/// ★★ The Samsung case: an equaliser copies the tallest measured height
/// onto every card. Real heights are heights at 1280 px, and the document
/// is read at other widths, so the write is REFUSED, and counted. A write
/// that does not copy a measurement is applied, and so is zero.
#[test]
fn an_equaliser_write_of_a_measured_height_is_refused() {
    let c = run("<html><head><style>body{margin:0} .row{display:flex} .card{width:300px}</style></head><body><div class=row>\
        <div class=card id=k1>short</div><div class=card id=k2>a longer text that wraps over several lines in a card three hundred px wide</div></div>\
        <div id=other></div><script>\
        var m = 0; document.querySelectorAll('.card').forEach(function (c) { m = Math.max(m, c.offsetHeight); });\
        document.querySelectorAll('.card').forEach(function (c) { c.style.minHeight = m + 'px'; });\
        document.getElementById('k1').style.setProperty('height', m + 'px');\
        document.getElementById('other').style.height = '123px';\
        document.getElementById('other').style.minHeight = '0px';\
        </script></body></html>");
    assert!(!c.html.contains("min-height") || c.html.contains("min-height: 0px"), "{}", c.html);
    assert!(!tag(&c.html, "k1").contains("style"), "{}", c.html);
    assert!(c.html.contains("height: 123px"), "an unmeasured value is applied: {}", c.html);
    assert_eq!(c.geo.writes_refused, 3, "{:?}", c.geo);
}

/// ★ IntersectionObserver delivers one initial record per target, from the
/// layout: the viewport at scroll 0, grown by rootMargin. (Unscrolled: the
/// sweep below would then scroll every target into view.)
#[test]
fn an_intersection_observer_reports_the_initial_intersections() {
    // Divs, not images: a test document has no network, and an image the
    // converter cannot size is dropped by the normalizer (profile §3.13), so
    // it has no box, here as in the lane.
    let c = run_unscrolled("<html><head><style>body{margin:0} .s{height:700px} .t{width:10px;height:10px} #i4{display:none}</style></head><body>\
        <div class=t id=i1></div><div class=s></div><div class=t id=i2></div><div class=s></div><div class=t id=i3></div>\
        <div class=t id=i4></div><script>\
        var n = 0;\
        var io = new IntersectionObserver(function (es) { es.forEach(function (e) { n++;\
            e.target.setAttribute('data-hit', e.isIntersecting ? 'y' : 'n'); e.target.setAttribute('data-ratio', String(e.intersectionRatio)); }); });\
        var wide = new IntersectionObserver(function (es) { es.forEach(function (e) { if (e.isIntersecting) e.target.setAttribute('data-wide', 'y'); }); },\
            { rootMargin: '200px 0px' });\
        ['i1', 'i2', 'i3', 'i4'].forEach(function (id) { io.observe(document.getElementById(id)); io.observe(document.getElementById(id));\
            wide.observe(document.getElementById(id)); });\
        setTimeout(function () { document.body.setAttribute('data-n', String(n)); }, 50);\
        </script></body></html>");
    assert_eq!(attr(&c.html, "i1", "data-hit"), "y");
    assert_eq!(attr(&c.html, "i1", "data-ratio"), "1");
    assert_eq!(attr(&c.html, "i2", "data-hit"), "n", "at y=710, below the 600px fold");
    assert_eq!(attr(&c.html, "i3", "data-hit"), "n");
    assert_eq!(attr(&c.html, "i4", "data-hit"), "n", "no box, no intersection");
    // 200px of root margin reaches 800: i2 at 710 is in, i3 at 1420 is not.
    assert_eq!(attr(&c.html, "i2", "data-wide"), "y");
    assert!(!tag(&c.html, "i3").contains("data-wide"), "{}", tag(&c.html, "i3"));
    assert!(c.html.contains(r#"data-n="4""#), "one record per target, observed twice: {}", c.html);
    assert_eq!(c.geo.io_records, 4 + 4);
}

#[test]
fn a_resize_observer_reports_the_content_box_once() {
    let c = run("<html><head><style>body{margin:0} #a{width:200px;height:30px;padding-left:10px;padding-right:10px} #z{width:0;height:0}</style></head>\
        <body><div id=a></div><div id=z></div><script>\
        var ro = new ResizeObserver(function (es) { es.forEach(function (e) {\
            e.target.setAttribute('data-c', [e.contentRect.width, e.contentRect.height, e.borderBoxSize[0].inlineSize].join(',')); }); });\
        ro.observe(document.getElementById('a')); ro.observe(document.getElementById('z'));\
        </script></body></html>");
    // `width: 200px` is the CONTENT box (content-box sizing, which the
    // normalizer compiles to border-box): content 200, border box 220.
    assert_eq!(attr(&c.html, "a", "data-c"), "200,30,220");
    assert!(!tag(&c.html, "z").contains("data-c"), "a 0x0 box gets no initial record");
    assert_eq!(c.geo.ro_records, 1);
}

/// A read after a mutation lays out again; past the budget the last
/// layout answers and the read is counted as stale.
#[test]
fn a_read_after_a_mutation_lays_out_again_within_a_budget() {
    let c = run("<html><head><style>body{margin:0}</style></head><body><div id=box></div><script>\
        var b = document.getElementById('box'), hs = [];\
        for (var i = 0; i < 20; i++) { var p = document.createElement('p'); p.textContent = 'line ' + i; b.appendChild(p); hs.push(b.offsetHeight); }\
        b.setAttribute('data-h', hs.join(','));\
        </script></body></html>");
    let hs: Vec<u32> = attr(&c.html, "box", "data-h").split(',').map(|v| v.parse().unwrap()).collect();
    assert_eq!(c.geo.layouts, MAX_LAYOUTS, "{:?}", c.geo);
    assert_eq!(c.geo.stale, 20 - MAX_LAYOUTS, "{:?}", c.geo);
    // Heights grow while layouts last, and hold at the last one after.
    for w in hs[..MAX_LAYOUTS as usize].windows(2) { assert!(w[1] > w[0], "{hs:?}") }
    assert!(hs[MAX_LAYOUTS as usize..].iter().all(|&h| h == hs[MAX_LAYOUTS as usize - 1]), "{hs:?}");
}

/// ★ Deterministic: the same document converts to the same bytes.
#[test]
fn two_conversions_are_identical() {
    let html = format!("<html><head>{BASE}</head><body><h2 id=a>A</h2><h2 id=b>B</h2><script>\
        document.querySelectorAll('h2').forEach(function (h) {{ h.setAttribute('data-r', JSON.stringify(h.getBoundingClientRect())); }});\
        </script></body></html>");
    assert_eq!(run(&html).html, run(&html).html);
}

/// With geometry off the old nominal answers come back (the A/B switch).
#[test]
fn geometry_off_is_the_old_nominal_box() {
    let c = convert("<html><body><div id=a>x</div><script>document.getElementById('a').setAttribute('data-w', String(document.getElementById('a').offsetWidth));</script></body></html>",
        &mut BoaEngine { nominal_geometry: true, ..Default::default() });
    assert_eq!(attr(&c.html, "a", "data-w"), "1280");
    assert_eq!(c.geo.layouts, 0);
}

const LAZY: &str = "<html><head><style>body{margin:0} .s{height:900px} .t{width:10px;height:10px}</style></head><body><h2 id=h1>One</h2>\
    <div class=t id=r1 data-src=a></div><div class=s></div><div class=t id=r2 data-src=b></div><div class=s></div>\
    <div class=t id=r3 data-src=c></div><div class=t id=o1 data-src=d></div><div class=s></div><div class=t id=o2 data-src=e></div>\
    <div class=s></div><h2 id=h2>Two</h2><script>\
    function rectLoad() { document.querySelectorAll('[id^=r][data-src]').forEach(function (e) {\
        var r = e.getBoundingClientRect(); if (r.top < innerHeight && r.bottom > 0) { e.setAttribute('data-loaded', e.dataset.src); e.removeAttribute('data-src'); } }); }\
    window.addEventListener('scroll', function () { setTimeout(rectLoad, 100); }); rectLoad();\
    var io = new IntersectionObserver(function (es) { es.forEach(function (e) {\
        if (e.isIntersecting) { e.target.setAttribute('data-loaded', e.target.dataset.src); io.unobserve(e.target); } }); });\
    document.querySelectorAll('[id^=o]').forEach(function (e) { io.observe(e); });\
    window.addEventListener('scroll', function () { var cur = null;\
        document.querySelectorAll('h2').forEach(function (h) { h.removeAttribute('data-active'); if (h.offsetTop <= scrollY + 600) cur = h; });\
        if (cur) cur.setAttribute('data-active', '1'); document.body.setAttribute('data-y', String(scrollY)); });\
    </script></body></html>";

/// ★★ THE READER'S SWEEP: at scroll 0 a lazy loader with honest geometry
/// loads the first screen only; a reader scrolls, and the recording keeps
/// what loads on the way — for a rect-on-scroll loader (Samsung's kind) and
/// an IntersectionObserver loader (The Hindu's kind) alike. The page is
/// back at the top when the recording is taken.
#[test]
fn the_sweep_loads_what_a_reader_would_scroll_to() {
    let still = run_unscrolled(LAZY);
    assert_eq!(attr(&still.html, "r1", "data-loaded"), "a");
    assert!(!tag(&still.html, "r2").contains("data-loaded") && !tag(&still.html, "o2").contains("data-loaded"));
    let c = run(LAZY);
    for (id, v) in [("r1", "a"), ("r2", "b"), ("r3", "c"), ("o1", "d"), ("o2", "e")] {
        assert_eq!(attr(&c.html, id, "data-loaded"), v, "{id}: {}", tag(&c.html, id));
    }
    // Back at the top: the scroll-spy's last word is about scroll 0.
    assert!(c.html.contains(r#"data-y="0""#), "{}", c.html);
    assert_eq!(attr(&c.html, "h1", "data-active"), "1");
    assert!(!tag(&c.html, "h2").contains("data-active"));
    assert!(c.geo.scroll_steps >= 4, "{:?}", c.geo);
}

/// A page that cannot notice a scroll is not scrolled (or laid out for it).
#[test]
fn a_page_without_scroll_listeners_is_not_swept() {
    let c = run("<html><body><div style='height:5000px'>x</div><script>var x = 1;</script></body></html>");
    assert_eq!((c.geo.scroll_steps, c.geo.layouts), (0, 0));
}

/// ★ A lazy placeholder with no `src` has a box, as in a browser: the
/// normalizer drops an unsized image from the DOCUMENT, but the layout that
/// answers scripts declares it 0 × 0 (Chrome: 300 × 0 under `width: 100%`,
/// one client rect). Samsung's loader asks `offsetWidth ||
/// getClientRects().length` before it loads; without a box nothing loaded.
#[test]
fn a_source_less_image_has_a_box() {
    let c = run("<html><head><style>body{margin:0} .w{width:300px} .w img{width:100%}</style></head><body>\
        <div class=w><img id=i1 data-src=x></div><img id=i3 data-src=x><p id=out></p><script>\
        var r = []; ['i1', 'i3'].forEach(function (id) { var e = document.getElementById(id);\
        r.push(e.offsetWidth + 'x' + e.offsetHeight + '/' + e.getClientRects().length); });\
        document.getElementById('out').setAttribute('data-b', r.join(' '));</script></body></html>");
    assert_eq!(attr(&c.html, "out", "data-b"), "300x0/1 0x0/1");
}
