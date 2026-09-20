use navigator_prerender::{boa_impl::BoaEngine, convert, convert_with, fetch::NoNetwork};

fn run(html: &str) -> navigator_prerender::Conversion {
    convert(html, &mut BoaEngine::default())
}

/// performance.now() must be a counter, not a clock: a wall-clock reading
/// would make the same document convert differently run to run.
#[test]
fn performance_now_is_deterministic_and_monotonic() {
    let html = "<html><body><div id=r></div><script>\
        var a = performance.now(), b = performance.now();\
        var e = document.getElementById('r');\
        e.setAttribute('mono', String(b > a));\
        e.setAttribute('first', String(a));\
        </script></body></html>";
    let one = run(html);
    let two = run(html);
    assert_eq!(one.scripts_failed, 0, "{:?}", one.errors);
    assert!(one.html.contains(r#"mono="true""#), "not monotonic: {}", one.html);
    assert_eq!(one.html, two.html, "performance.now() made the conversion non-deterministic");
}

/// Storage exists, works within a conversion, and starts EMPTY every time —
/// which is what keeps it a stub rather than a capability.
#[test]
fn storage_works_but_never_persists() {
    let html = "<html><body><div id=r></div><script>\
        var before = localStorage.getItem('k');\
        localStorage.setItem('k','v');\
        var e = document.getElementById('r');\
        e.setAttribute('before', String(before));\
        e.setAttribute('after', localStorage.getItem('k'));\
        e.setAttribute('len', String(localStorage.length));\
        </script></body></html>";
    let a = run(html);
    assert_eq!(a.scripts_failed, 0, "{:?}", a.errors);
    assert!(a.html.contains(r#"before="null""#), "must start empty: {}", a.html);
    assert!(a.html.contains(r#"after="v""#) && a.html.contains(r#"len="1""#));
    // a second conversion must not see the first one's writes
    let b = run(html);
    assert_eq!(a.html, b.html, "storage leaked between conversions");
}

/// There are no cookies — reads are empty and writes are dropped, rather
/// than the property being absent and throwing.
#[test]
fn cookies_are_absent_not_broken() {
    let html = "<html><body><div id=r></div><script>\
        document.cookie = 'a=1';\
        document.getElementById('r').setAttribute('c', String(document.cookie));\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"c="""#), "cookie must read empty: {}", c.html);
}

#[test]
fn location_comes_from_the_document_url() {
    let html = "<html><body><div id=r></div><script>\
        var e = document.getElementById('r');\
        e.setAttribute('host', location.hostname);\
        e.setAttribute('path', location.pathname);\
        e.setAttribute('proto', location.protocol);\
        </script></body></html>";
    let c = convert_with(html, Some("https://example.test/a/b.html?q=1"),
                         &mut BoaEngine::default(), &mut NoNetwork);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"host="example.test""#), "got {}", c.html);
    assert!(c.html.contains(r#"path="/a/b.html""#), "got {}", c.html);
    assert!(c.html.contains(r#"proto="https:""#), "got {}", c.html);
}

#[test]
fn url_constructor_resolves_against_a_base() {
    let html = "<html><body><div id=r></div><script>\
        var u = new URL('../x.js', 'https://h.test/a/b/c.html');\
        document.getElementById('r').setAttribute('u', u.href);\
        var bad = false; try { new URL('::::'); } catch (e) { bad = true; }\
        document.getElementById('r').setAttribute('threw', String(bad));\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"u="https://h.test/a/x.js""#), "got {}", c.html);
    assert!(c.html.contains(r#"threw="true""#), "invalid URL must throw: {}", c.html);
}

/// navigator identifies the CONVERTER, not a reader's machine.
#[test]
fn navigator_is_fixed_not_host_derived() {
    let html = "<html><body><div id=r></div><script>\
        document.getElementById('r').setAttribute('ua', navigator.userAgent);\
        </script></body></html>";
    let c = run(html);
    assert!(c.html.contains("atrium-navigator-prerender"), "got {}", c.html);
}

/// currentScript must point at the executing element, and its src must be
/// ABSOLUTE as a browser reports it — webpack derives publicPath from it.
#[test]
fn current_script_points_at_the_running_element() {
    use navigator_prerender::fetch::MapFetcher;
    let mut f = MapFetcher::default();
    f.0.insert("https://example.test/assets/app.js".into(),
        "document.body.setAttribute('src', document.currentScript.src);\
         document.body.setAttribute('tag', document.currentScript.tagName);".into());
    let html = r#"<html><body><script src="/assets/app.js"></script></body></html>"#;
    let c = convert_with(html, Some("https://example.test/page.html"),
                         &mut BoaEngine::default(), &mut f);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"src="https://example.test/assets/app.js""#),
        "src must be absolute: {}", c.html);
    assert!(c.html.contains(r#"tag="SCRIPT""#), "got {}", c.html);
}

/// Null for a module, as the spec requires, and null once scripts are done.
#[test]
fn current_script_is_null_for_modules_and_after_scripts() {
    let html = r#"<html><body><div id=r></div>
      <script type="module">
        document.getElementById('r').setAttribute('in-module', String(document.currentScript));
      </script>
      <script>
        document.addEventListener('DOMContentLoaded', function () {
          document.getElementById('r').setAttribute('in-handler', String(document.currentScript));
        });
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"in-module="null""#), "module must see null: {}", c.html);
    assert!(c.html.contains(r#"in-handler="null""#), "handler must see null: {}", c.html);
}

/// An inline script still gets an element, just one with no src.
#[test]
fn current_script_for_inline_has_no_src() {
    let html = "<html><body><div id=r></div><script>\
        document.getElementById('r').setAttribute('t', document.currentScript.tagName);\
        document.getElementById('r').setAttribute('s', String(document.currentScript.src));\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"t="SCRIPT""#), "got {}", c.html);
    assert!(c.html.contains(r#"s="undefined""#), "inline has no src: {}", c.html);
}

/// Geometry observers must be constructible and register, but never deliver:
/// there is no layout here, so any box handed to a callback is fiction, and
/// a script told an element is 0x0 routinely hides it.
#[test]
fn geometry_observers_accept_but_never_fire() {
    let html = "<html><body><div id=r>kept</div><script>\
        var fired = 0;\
        var ro = new ResizeObserver(function(){ fired++; });\
        ro.observe(document.getElementById('r'));\
        var io = new IntersectionObserver(function(){ fired++; });\
        io.observe(document.getElementById('r'));\
        document.getElementById('r').setAttribute('fired', String(fired));\
        ro.disconnect();\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"fired="0""#), "must not deliver: {}", c.html);
    assert!(c.html.contains("kept"), "content must survive: {}", c.html);
    assert_eq!(c.observers_registered, 2, "registrations must be counted");
}

/// A lookup that finds nothing is attributed with its argument, and the two
/// causes are kept apart: a selector WE cannot parse is our gap, one that
/// parses and matches nothing is the document's.
#[test]
fn null_lookups_are_attributed_and_the_cause_separated() {
    let html = "<html><body><p>only</p><script>\
        try { document.getElementById('nope').x = 1; } catch (e) {}\
        try { document.querySelector('.absent'); } catch (e) {}\
        try { document.querySelector('###'); } catch (e) {}\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    let names: Vec<&str> = c.nulls.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"getElementById(nope)"), "got {names:?}");
    assert!(names.contains(&"querySelector-no-match(.absent)"), "got {names:?}");
    assert!(names.contains(&"querySelector-UNPARSEABLE(###)"), "got {names:?}");
}

/// The control arm: a document is classified by its FIRST failure, because
/// later ones may cascade from it.
#[test]
fn verdicts_separate_our_gaps_from_failures_a_browser_shares() {
    use navigator_prerender::Verdict;

    // No failure at all.
    let clean = convert("<html><body><script>var a=1;</script></body></html>",
                        &mut BoaEngine::default());
    assert_eq!(clean.verdict, Verdict::Clean);

    // A missing binding is ours.
    let ours = convert("<html><body><script>noSuchGlobal.go();</script></body></html>",
                       &mut BoaEngine::default());
    assert_eq!(ours.verdict, Verdict::OurGap, "{:?}", ours.first_error);

    // Querying for an element this page does not contain, then using the
    // null, is what a real browser does too.
    let browser = convert(
        "<html><body><p>only</p><script>document.querySelector('.absent').focus();</script></body></html>",
        &mut BoaEngine::default());
    assert_eq!(browser.verdict, Verdict::BrowserToo, "{:?}", browser.first_error);

    // A selector WE cannot parse is ours, even though the symptom is a null.
    let unparseable = convert(
        "<html><body><script>document.querySelector('###').focus();</script></body></html>",
        &mut BoaEngine::default());
    assert_eq!(unparseable.verdict, Verdict::OurGap, "{:?}", unparseable.first_error);
}

/// Attribution must be CAUSAL: the last event before the throw decides, not a
/// document-wide tally. A miss recorded by an unrelated script that ran fine
/// must not outvote the real proximate cause.
#[test]
fn attribution_uses_the_proximate_cause_not_a_tally() {
    use navigator_prerender::Verdict;
    // An earlier, harmless miss, then a throw caused by a no-match lookup.
    let html = "<html><body><p>only</p><script>\
        var ignored = document.images;\
        document.querySelector('.absent').focus();\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert!(c.missing.iter().any(|(n, _)| n == "document.images"),
        "the harmless miss must still be recorded: {:?}", c.missing);
    assert_eq!(c.verdict, Verdict::BrowserToo,
        "a tally would have said OurGap; cause was {:?}", c.cause);
    assert_eq!(c.cause.as_ref().map(|(k, _)| k.as_str()), Some("no-match"));
}

/// Layout metrics are fiction; the test pins WHICH fiction — a generous,
/// self-consistent box that errs toward content being visible, with scroll*
/// equal to client* so nothing concludes it must truncate.
#[test]
fn layout_metrics_are_nominal_consistent_and_counted() {
    let html = "<html><body><div id=r>x</div><script>\
        var e = document.getElementById('r');\
        e.setAttribute('ch', String(e.clientHeight));\
        e.setAttribute('sh', String(e.scrollHeight));\
        e.setAttribute('cw', String(e.clientWidth));\
        var rect = e.getBoundingClientRect();\
        e.setAttribute('rw', String(rect.width));\
        e.setAttribute('overflows', String(e.scrollHeight > e.clientHeight));\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"overflows="false""#),
        "scroll must equal client so nothing truncates: {}", c.html);
    assert!(!c.html.contains(r#"ch="0""#), "zero height makes scripts hide content: {}", c.html);
    assert!(c.html.contains(r#"rw="1280""#), "got {}", c.html);
    assert!(c.layout_reads >= 5, "reads must be counted, got {}", c.layout_reads);
}

/// ★ The converter's timezone is UTC, not the host machine's. Boa's default
/// hook reports the local offset of whatever box is running, so this returned
/// -330 (IST) here and would return something else elsewhere — baking the
/// converter's location into the artifact and making the same document
/// convert differently on two machines. Same decision as the fixed user agent
/// and viewport: fixed identity, the converter's and not the reader's.
#[test]
fn timezone_is_utc_not_the_host_machines() {
    let html = "<html><body><div id=t></div><script>\
        document.getElementById('t').setAttribute('tz',\
          String(new Date(0).getTimezoneOffset()));\
        document.getElementById('t').setAttribute('s',\
          new Date(0).toISOString());\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"tz="0""#), "host timezone leaked: {}", c.html);
    assert!(c.html.contains(r#"s="1970-01-01T00:00:00.000Z""#), "{}", c.html);
}

/// ★ Intl exists. It was reported "blocked upstream" on the strength of a
/// STALE local crates.io index — icu_list 2.3.0 is published, and
/// `intl_bundled` builds fine once the index is refreshed. Without it there
/// is no Intl at all and toLocaleDateString silently degrades to
/// `Date.toString()`, so a page formatting a date gets garbage.
#[test]
fn intl_is_present_and_really_formats() {
    let html = "<html><body><div id=t></div><script>\
        document.getElementById('t').setAttribute('a', typeof Intl);\
        document.getElementById('t').setAttribute('b', new Intl.NumberFormat('de-DE').format(1234.5));\
        document.getElementById('t').setAttribute('c', new Intl.DateTimeFormat('en-GB').format(new Date(0)));\
        document.getElementById('t').setAttribute('d', 'i'.toLocaleUpperCase('tr'));\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"a="object""#), "{}", c.html);
    // Real ICU data, not a stub: German grouping, British date order, and
    // the Turkish dotted capital I.
    assert!(c.html.contains(r#"b="1.234,5""#), "{}", c.html);
    assert!(c.html.contains(r#"c="01/01/1970""#), "{}", c.html);
    assert!(c.html.contains("d=\"\u{130}\""), "{}", c.html);
}

/// ★ An omitted locale resolves to the DOCUMENT's, never the host machine's.
/// boa asks sys_locale, so this formatted as en-IN purely because the
/// developer's machine is — a host-state leak that enabling Intl would
/// otherwise have introduced. A German page's numbers should read German
/// wherever the conversion runs.
#[test]
fn omitted_locale_comes_from_the_document_not_the_host() {
    let html = "<html lang=\"de-DE\"><body><div id=t></div><script>\
        document.getElementById('t').setAttribute('n', (1234.5).toLocaleString());\
        document.getElementById('t').setAttribute('l', new Intl.NumberFormat().resolvedOptions().locale);\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"n="1.234,5""#), "document lang ignored: {}", c.html);
    // ★ RECORDED DIVERGENCE: resolvedOptions().locale reports ICU's
    // MINIMIZED tag ("de"), where a browser echoes the full "de-DE". The
    // formatting is identical; only the reported tag differs. Asserted as it
    // actually behaves so the difference is documented rather than papered
    // over — I expected "de-DE" and was wrong about the engine, not the code.
    assert!(c.html.contains(r#"l="de""#), "{}", c.html);
}

/// With no lang declared the default is a FIXED en-US — a recorded choice,
/// not whatever the converter's OS is set to.
#[test]
fn without_a_declared_language_the_default_is_fixed() {
    let html = "<html><body><div id=t></div><script>\
        document.getElementById('t').setAttribute('l', new Intl.DateTimeFormat().resolvedOptions().locale);\
        document.getElementById('t').setAttribute('z', new Intl.DateTimeFormat().resolvedOptions().timeZone);\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert!(c.html.contains(r#"l="en-US""#), "host locale leaked: {}", c.html);
    assert!(c.html.contains(r#"z="utc""#), "host timezone leaked: {}", c.html);
}

/// An EXPLICIT locale is always passed through untouched — the defaulting
/// must never override what the page actually asked for.
#[test]
fn an_explicit_locale_is_never_overridden() {
    let html = "<html lang=\"de-DE\"><body><div id=t></div><script>\
        document.getElementById('t').setAttribute('n', (1234.5).toLocaleString('en-US'));\
        document.getElementById('t').setAttribute('m', new Intl.NumberFormat('fr-FR').resolvedOptions().locale);\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert!(c.html.contains(r#"n="1,234.5""#), "{}", c.html);
    assert!(c.html.contains(r#"m="fr""#), "minimized tag — see the note above: {}", c.html);
}
