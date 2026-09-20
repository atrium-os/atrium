use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn run(html: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some("https://example.test/p.html"),
                 &mut BoaEngine::default(), &mut NoNetwork)
}

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ WHERE IT WRITES IS THE WHOLE QUESTION. A browser inserts at the parser's
/// position, which during a script is immediately after that script element.
/// currentScript names the element, so this case is exact rather than
/// approximate.
#[test]
fn write_lands_after_the_running_script() {
    let c = run(r#"<html><body><p id=before>b</p><script>
      document.write('<span id=w>written</span>');
    </script><p id=after>a</p></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(c.doc_writes, 1);
    let b = c.html.find("id=\"before\"").unwrap();
    let w = c.html.find("id=\"w\"").unwrap();
    let a = c.html.find("id=\"after\"").unwrap();
    assert!(b < w && w < a, "written content is out of order: {}", c.html);
}

/// Several writes from one script keep their order.
#[test]
fn successive_writes_keep_document_order() {
    let c = run(r#"<html><body><script>
      document.write('<i>one</i>');
      document.write('<i>two</i>');
      document.writeln('<i>three</i>');
    </script></body></html>"#);
    assert_eq!(c.doc_writes, 3);
    let (o, t, h) = (c.html.find(">one<").unwrap(),
                     c.html.find(">two<").unwrap(),
                     c.html.find(">three<").unwrap());
    assert!(o < t && t < h, "{}", c.html);
}

/// Written markup goes through the real parser, so it is structure, not text.
#[test]
fn written_markup_is_parsed_not_escaped() {
    let c = run(r#"<html><body><div id=host></div><script>
      document.write('<ul class="k"><li>x</li><li>y</li></ul>');
    </script></body></html>"#);
    // ★ Look only AFTER the script element: the artifact carries the script
    // source too, and that source contains the escaped markup as a string
    // literal. Asserting over the whole document matches the literal and
    // reports the opposite of the truth.
    let after = &c.html[c.html.find("</script>").unwrap()..];
    assert!(after.contains(r#"<ul class="k"><li>x</li><li>y</li></ul>"#), "{}", c.html);
    assert!(!after.contains("&lt;ul"), "markup was escaped instead of parsed: {}", c.html);
}

/// ★ Called with no script running, a browser performs an implicit
/// document.open() that ERASES the document. A converter must not destroy
/// the artifact, so it is refused — and COUNTED, so the choice is visible
/// rather than silent.
#[test]
fn a_write_that_would_erase_the_document_is_refused_and_counted() {
    let c = run(r#"<html><body><p id=keep>kept</p><script>
      setTimeout(function () { document.write('<b>too late</b>'); }, 0);
    </script></body></html>"#);
    assert_eq!(c.doc_writes, 0);
    assert_eq!(c.doc_writes_refused, 1, "the refusal must be counted");
    assert!(c.html.contains(">kept<"), "the document must survive: {}", c.html);
    assert!(!c.html.contains("too late</b>"), "{}", c.html);
}

/// The honest referrer is empty: no navigation happened, which is exactly
/// what a browser reports for a directly-opened document.
#[test]
fn referrer_is_empty_because_no_navigation_happened() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r',
        JSON.stringify(document.referrer) + '|' + typeof document.referrer);
    </script></body></html>"#);
    assert_eq!(r(&c), "&quot;&quot;|string");
}

/// ★ A deliberate probe is not a proximate cause. core-js reads document.all
/// to detect the IsHTMLDDA quirk and EXPECTS to find nothing; blaming a later,
/// unrelated failure on it misattributes the gap. It stays in the
/// most-wanted list (pages do ask for it) but out of the causal log.
#[test]
fn detection_probes_are_counted_but_never_blamed() {
    let c = run(r#"<html><body><p>only</p><script>
      var all = typeof document == 'object' && document.all;   // expects absence
      document.querySelector('.absent').focus();               // the real failure
    </script></body></html>"#);
    use navigator_prerender::Verdict;
    assert!(c.missing.iter().any(|(n, _)| n == "document.all"),
        "the probe must still be reported as a miss: {:?}", c.missing);
    assert_eq!(c.cause.as_ref().map(|(k, _)| k.as_str()), Some("no-match"),
        "the probe must not be the cause; got {:?}", c.cause);
    assert_eq!(c.verdict, Verdict::BrowserToo,
        "a browser fails this too; got {:?} via {:?}", c.verdict, c.cause);
}

/// core-js's actual IsHTMLDDA expression must evaluate without throwing and
/// select the standard path, which is what every non-browser host does.
#[test]
fn the_is_htmldda_probe_takes_the_standard_path() {
    let c = run(r#"<html><body><div id=t></div><script>
      var documentAll = typeof document == 'object' && document.all;
      var IS_HTMLDDA = typeof documentAll == 'undefined' && documentAll !== undefined;
      document.body.setAttribute('data-r', String(IS_HTMLDDA) + '|' + String(!!documentAll));
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(r(&c), "false|false");
}
