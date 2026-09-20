use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn run(html: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some("https://example.test/docs/page.html?x=1"),
                 &mut BoaEngine::default(), &mut NoNetwork)
}

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ The PROPERTY is absolute even when the ATTRIBUTE is relative. That is
/// the difference the whole feature turns on, and it is why `href` was the
/// most-requested property in the corpus at 3089 hits.
#[test]
fn href_property_is_absolute_attribute_stays_relative() {
    let c = run(r#"<html><body><a id=t href="../other.html?q=2#frag">x</a><script>
      var a = document.getElementById('t');
      document.body.setAttribute('data-r', a.href + ' || ' + a.getAttribute('href'));
    </script></body></html>"#);
    assert_eq!(r(&c),
        "https://example.test/other.html?q=2#frag || ../other.html?q=2#frag");
}

/// Each part as the DOM defines it: protocol keeps its colon, search keeps
/// its `?`, hash keeps its `#`, and port is empty when it is the default.
#[test]
fn the_parts_carry_their_punctuation() {
    let c = run(r#"<html><body><a id=t href="https://sub.example.org:8443/a/b?k=v#top">x</a><script>
      var a = document.getElementById('t');
      document.body.setAttribute('data-r', [
        a.protocol, a.host, a.hostname, a.port, a.pathname, a.search, a.hash, a.origin
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c),
        "https:|sub.example.org:8443|sub.example.org|8443|/a/b|?k=v|#top|https://sub.example.org:8443");
}

#[test]
fn default_port_and_absent_parts_are_empty_strings() {
    let c = run(r#"<html><body><a id=t href="https://example.test/plain">x</a><script>
      var a = document.getElementById('t');
      document.body.setAttribute('data-r', [
        JSON.stringify(a.port), JSON.stringify(a.search), JSON.stringify(a.hash), a.pathname
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "&quot;&quot;|&quot;&quot;|&quot;&quot;|/plain");
}

/// An <a> with no href reports empty strings — not undefined, and not a
/// throw. Pages read these without checking.
#[test]
fn a_link_without_href_reports_empty_not_undefined() {
    let c = run(r#"<html><body><a id=t>x</a><script>
      var a = document.getElementById('t');
      document.body.setAttribute('data-r',
        typeof a.href + '|' + JSON.stringify(a.href) + '|' + JSON.stringify(a.protocol));
    </script></body></html>"#);
    assert_eq!(r(&c), "string|&quot;&quot;|&quot;&quot;");
}

/// ★ Only hyperlink elements have these. On anything else they are
/// undefined, and answering everywhere would resolve a feature detection the
/// wrong way.
#[test]
fn non_hyperlink_elements_do_not_have_url_parts() {
    let c = run(r#"<html><body><div id=d href="/x"></div><a id=a href="/x"></a><script>
      document.body.setAttribute('data-r',
        typeof document.getElementById('d').protocol + '|' +
        typeof document.getElementById('a').protocol);
    </script></body></html>"#);
    assert_eq!(r(&c), "undefined|string");
}

/// Assigning href writes the ATTRIBUTE, so the artifact carries it and every
/// part recomputes from it. A plain JS property would diverge from the markup.
#[test]
fn assigning_href_writes_the_attribute_and_parts_follow() {
    let c = run(r#"<html><body><a id=t href="/old">x</a><script>
      var a = document.getElementById('t');
      a.href = 'https://other.test:9000/new?z=1';
      document.body.setAttribute('data-r',
        a.getAttribute('href') + '|' + a.hostname + '|' + a.port + '|' + a.search);
    </script></body></html>"#);
    assert_eq!(r(&c), "https://other.test:9000/new?z=1|other.test|9000|?z=1");
    assert!(c.html.contains(r#"<a id="t" href="https://other.test:9000/new?z=1">"#),
        "the attribute must reach the artifact: {}", c.html);
}

/// The parts are live against setAttribute, not a snapshot taken when the
/// wrapper was built.
#[test]
fn parts_are_live_against_setattribute() {
    let c = run(r#"<html><body><a id=t href="/first">x</a><script>
      var a = document.getElementById('t');
      var before = a.pathname;
      a.setAttribute('href', '/second');
      document.body.setAttribute('data-r', before + '|' + a.pathname);
    </script></body></html>"#);
    assert_eq!(r(&c), "/first|/second");
}

/// link and area are hyperlink elements too — stylesheet loaders read
/// link.href constantly.
#[test]
fn link_and_area_also_decompose() {
    let c = run(r#"<html><head><link id=l rel=stylesheet href="/css/a.css"></head>
      <body><map><area id=m href="/m/x"></map><script>
      document.body.setAttribute('data-r',
        document.getElementById('l').pathname + '|' +
        document.getElementById('m').pathname);
    </script></body></html>"#);
    assert_eq!(r(&c), "/css/a.css|/m/x");
}
