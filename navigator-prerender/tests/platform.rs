use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn run_at(html: &str, url: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some(url), &mut BoaEngine::default(), &mut NoNetwork)
}
fn run(html: &str) -> navigator_prerender::Conversion {
    run_at(html, "https://example.test/p.html")
}
fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ RANDOMNESS IS SEEDED FROM THE DOCUMENT, NOT THE MACHINE. A converter
/// feeding a content-addressed store must produce the SAME artifact from the
/// same input; real entropy would give every conversion a different hash and
/// defeat dedup entirely.
#[test]
fn crypto_is_reproducible_for_the_same_document() {
    let html = r#"<html><body><div id=t></div><script>
      var a = new Uint32Array(3); crypto.getRandomValues(a);
      document.body.setAttribute('data-r',
        Array.from(a).join(',') + '|' + crypto.randomUUID());
    </script></body></html>"#;
    let one = run(html);
    let two = run(html);
    assert_eq!(r(&one), r(&two), "the same document must convert identically");
}

/// And a DIFFERENT document gets a different stream, so the determinism is
/// per-document rather than a single constant.
#[test]
fn a_different_document_gets_a_different_stream() {
    let html = r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r', crypto.randomUUID());
    </script></body></html>"#;
    let a = run_at(html, "https://example.test/one.html");
    let b = run_at(html, "https://example.test/two.html");
    assert_ne!(r(&a), r(&b));
}

/// The UUID must still parse as one — callers slice and validate it.
#[test]
fn random_uuid_has_the_right_shape() {
    let c = run(r#"<html><body><div id=t></div><script>
      var u = crypto.randomUUID();
      var ok = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u);
      document.body.setAttribute('data-r', ok + '|' + u.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "true|36");
}

/// subtle is ABSENT rather than stubbed: a page that needs real cryptography
/// must find out, not trust a fake.
#[test]
fn crypto_subtle_is_absent_not_faked() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r',
        typeof crypto + '|' + typeof crypto.subtle + '|' + typeof crypto.getRandomValues);
    </script></body></html>"#);
    assert_eq!(r(&c), "object|undefined|function");
}

/// ★ A specific interface must be SPECIFIC. Answering from nodeType alone
/// would make every element every interface — a worse lie than absence.
#[test]
fn element_interfaces_are_tag_specific() {
    let c = run(r#"<html><head><link id=l rel=stylesheet href=/a.css></head>
      <body><div id=d></div><form id=f></form><script>
      var s = document.scripts[0], l = document.getElementById('l');
      var d = document.getElementById('d'), f = document.getElementById('f');
      document.body.setAttribute('data-r', [
        s instanceof HTMLScriptElement, d instanceof HTMLScriptElement,
        l instanceof HTMLLinkElement,   d instanceof HTMLLinkElement,
        f instanceof HTMLFormElement,
        d instanceof HTMLElement
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "true|false|true|false|true|true");
}

#[test]
fn document_collections_are_live_html_collections() {
    let c = run(r#"<html><body>
      <form id=f1></form><img src=a.png><a href=/x>link</a><a>no href</a>
      <script>
        var before = document.forms.length;
        document.body.appendChild(document.createElement('form'));
        document.body.setAttribute('data-r', [
          before, document.forms.length, document.images.length,
          document.links.length,
          Object.prototype.toString.call(document.forms)
        ].join('|'));
      </script></body></html>"#);
    // An <a> without href is not a link, as the DOM defines it.
    assert_eq!(r(&c), "1|2|1|1|[object HTMLCollection]");
}

/// Scroll position and screen are FIXED, for the same reason the user agent
/// is: the reader's display is not the converter's to report.
#[test]
fn scroll_and_screen_are_fixed_not_host_derived() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r', [
        pageXOffset, pageYOffset, scrollX, scrollY,
        screen.width, screen.height, screen.colorDepth
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "0|0|0|0|1280|800|24");
}

/// Legacy detection globals are read HOPING for absence, so they must never
/// be blamed as a cause — window.opera has not existed since 2013.
#[test]
fn legacy_detection_globals_are_not_causes() {
    let c = run(r#"<html><body><p>only</p><script>
      var old = window.opera;                      // expects absence
      document.querySelector('.absent').focus();   // the real failure
    </script></body></html>"#);
    use navigator_prerender::Verdict;
    assert_eq!(c.cause.as_ref().map(|(k, _)| k.as_str()), Some("no-match"),
        "a legacy probe must not be the cause; got {:?}", c.cause);
    assert_eq!(c.verdict, Verdict::BrowserToo);
}
