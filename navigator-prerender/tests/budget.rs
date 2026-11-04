//! The exploration budget (legacy-web §5.4.5.6a item 2, §5.4.5.6b): every
//! distinct handler is probed once before any is probed twice; a handler
//! stops at its per-handler bound and a document at its total bound.

use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork, Conversion};

fn explore_with(html: &str, bounds: Option<(usize, usize)>) -> Conversion {
    let mut eng = BoaEngine { explore: true, probe_bounds: bounds, ..Default::default() };
    convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork)
}
fn plain(html: &str) -> Conversion {
    convert_with(html, Some("https://example.test/p.html"), &mut BoaEngine::default(), &mut NoNetwork)
}

/// `n` buttons sharing ONE handler (one source, a closure per button), then
/// `distinct` buttons each with its own handler source.
fn shared_then_distinct(n: usize, distinct: usize) -> String {
    let mut h = String::from("<html><body>");
    for i in 0..n { h.push_str(&format!("<button id=s{i}>s</button>")); }
    for i in 0..distinct { h.push_str(&format!("<button id=d{i}>d</button>")); }
    h.push_str("<p id=o></p><script>var O = document.getElementById('o');\n");
    h.push_str(&format!("for (var i = 0; i < {n}; i++) (function (i) {{ \
        document.getElementById('s' + i).addEventListener('click', function () {{ O.setAttribute('data-s', '' + i); }}); }})(i);\n"));
    for i in 0..distinct {
        // A different source per handler.
        h.push_str(&format!("document.getElementById('d{i}').addEventListener('click', function () {{ O.setAttribute('data-d{i}', 'x'); }});\n"));
    }
    h.push_str("</script></body></html>");
    h
}

/// ★ The flat cap probed in handle order, so 300 buttons sharing one handler
/// used up every probe and the five distinct handlers after them were never
/// reached. Round-robin reaches all five in the first round.
#[test]
fn every_distinct_handler_is_probed_before_any_is_probed_twice() {
    let html = shared_then_distinct(300, 5);
    let c = explore_with(&html, Some((4, 10)));
    let rec: Vec<&str> = c.transitions.iter().map(|t| t.trigger.as_str()).collect();
    for i in 0..5 { assert!(rec.contains(&format!("#d{i}").as_str()), "#d{i} not reached: {rec:?}") }
    let shared = rec.iter().filter(|t| t.starts_with("#s")).count();
    // Round 0: s0 + d0..d4 (6); rounds 1-3: s1, s2, s3 (9 in all, under 10).
    assert_eq!(shared, 4, "the shared handler stops at its bound: {rec:?}");
    assert_eq!(c.explore_stats.probes, 9);
    assert_eq!(c.explore_stats.handlers, 6);
    assert_eq!(c.explore_stats.cut, 296);
    assert_eq!(c.html, plain(&html).html, "the budget changes what is probed, never the artifact");
}

/// Elements cut under a handler that WAS probed are `represented`; the
/// recording does not name them, the coverage record says why.
#[test]
fn cut_elements_whose_handler_was_probed_are_represented() {
    let c = explore_with(&shared_then_distinct(50, 0), None);
    assert_eq!(c.transitions.len(), 8, "PROBES_PER_HANDLER");
    let cut: Vec<_> = c.coverage.iter().filter(|k| k.over_cap).collect();
    assert_eq!(cut.len(), 42);
    assert!(cut.iter().all(|k| k.represented && !k.probed), "{:?}", cut.first());
    assert!(c.coverage.iter().filter(|k| k.probed).all(|k| !k.represented));
}

/// The per-document bound holds however many distinct handlers there are,
/// and what it cuts is NOT represented: those handlers never ran.
#[test]
fn the_document_bound_holds_for_many_distinct_handlers() {
    let mut h = String::from("<html><body>");
    for i in 0..400 { h.push_str(&format!("<button id=b{i}>b</button>")); }
    h.push_str("<p id=o></p><script>for (var i = 0; i < 400; i++) document.getElementById('b' + i)\
        .addEventListener('click', new Function(\"document.getElementById('o').setAttribute('data-n', '\" + i + \"');\"));</script></body></html>");
    let c = explore_with(&h, None);
    assert_eq!(c.explore_stats.handlers, 400, "new Function sources differ");
    assert_eq!(c.explore_stats.probes, 256, "MAX_PROBES");
    assert_eq!(c.transitions.len(), 256);
    assert_eq!(c.explore_stats.rounds, 1, "the bound ends the first round");
    let cut: Vec<_> = c.coverage.iter().filter(|k| k.over_cap).collect();
    assert_eq!(cut.len(), 144);
    assert!(cut.iter().all(|k| !k.represented), "{:?}", cut.first());
}

/// Delegated candidates are budgeted the same way: a document listener
/// standing for 500 buttons is one handler key per what chose them.
#[test]
fn delegated_candidates_share_their_roots_budget() {
    let mut h = String::from("<html><body>");
    for i in 0..500 { h.push_str(&format!("<button class=t id=b{i}>b</button>")); }
    h.push_str("<p id=o></p><script>document.addEventListener('click', function (e) { \
        var b = e.target.closest('.t'); if (b) document.getElementById('o').setAttribute('data-b', b.id); });</script></body></html>");
    let c = explore_with(&h, None);
    // The discovery probe at <body>, then 8 of the 500 targets.
    assert_eq!(c.explore_stats.probes, 9, "{:?}", c.explore_stats);
    assert_eq!(c.transitions.len(), 8);
}

/// Bound (and native) functions are told apart by OBJECT, since their source
/// says nothing: ten bound handlers are ten handlers, each probed.
#[test]
fn bound_functions_are_distinct_handlers() {
    let mut h = String::from("<html><body>");
    for i in 0..10 { h.push_str(&format!("<button id=b{i}>b</button>")); }
    h.push_str("<p id=o></p><script>function f(i) { document.getElementById('o').setAttribute('data-n', i); }\n\
        for (var i = 0; i < 10; i++) document.getElementById('b' + i).addEventListener('click', f.bind(null, i));</script></body></html>");
    let c = explore_with(&h, Some((1, 256)));
    assert_eq!(c.explore_stats.handlers, 10);
    assert_eq!(c.transitions.len(), 10);
}

/// The same document explores the same way under the budget.
#[test]
fn the_budgeted_recording_is_reproducible() {
    let html = shared_then_distinct(40, 7);
    let (a, b) = (explore_with(&html, Some((3, 20))), explore_with(&html, Some((3, 20))));
    assert_eq!(format!("{:?}", a.transitions), format!("{:?}", b.transitions));
}

/// ★★ A handler that busy-waits on the clock never returns here, because
/// the converter's clock is FIXED (chase.com's link handler did, and timed
/// the document out). During exploration the engine's loop limit ends the
/// probe; the handler gets no second probe, the others are still probed,
/// and the artifact is untouched.
#[test]
fn a_spinning_handler_ends_its_probe_not_the_conversion() {
    let html = r#"<html><body><a id=s href=/x>spin</a><a id=s2 href=/y>spin2</a><button id=b>b</button><p id=o></p><script>
      function spin() { var t = Date.now() + 50; while (Date.now() < t) {} }
      document.getElementById('s').addEventListener('click', spin);
      document.getElementById('s2').addEventListener('click', spin);
      document.getElementById('b').addEventListener('click', function () {
        document.getElementById('o').setAttribute('class', 'on'); });
    </script></body></html>"#;
    let t = std::time::Instant::now();
    let c = explore_with(html, None);
    assert!(t.elapsed() < std::time::Duration::from_secs(20), "{:?}", t.elapsed());
    assert_eq!(c.explore_stats.aborted, 1, "{:?}", c.explore_stats);
    let s = c.coverage.iter().find(|k| k.path == "#s").unwrap();
    assert!(s.aborted, "{s:?}");
    let s2 = c.coverage.iter().find(|k| k.path == "#s2").unwrap();
    assert!(!s2.probed && !s2.represented, "a spinning handler gets no second probe: {s2:?}");
    assert_eq!(c.transitions.len(), 1, "{:?}", c.transitions);
    assert_eq!(c.html, plain(html).html);
}

/// The limit is exploration's alone: a long loop at LOAD still runs to the
/// end, so what the page builds is unchanged.
#[test]
fn the_load_pass_has_no_loop_limit() {
    let html = r#"<html><body><p id=o></p><script>
      var n = 0; for (var i = 0; i < 3000000; i++) n++;
      document.getElementById('o').textContent = 'n=' + n;
    </script></body></html>"#;
    let c = explore_with(html, None);
    assert!(c.html.contains("n=3000000"), "{}", c.html);
}
