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
