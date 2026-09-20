use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ THIS ONE HAD TO BE RIGHT OR NOT AT ALL. Its corpus uses are GUARDED
/// feature detections, so libraries already fall back correctly when it is
/// absent. A hand-written English implementation would flip those guards to
/// true and push English wording into German and French pages — worse than
/// the absence. So it is built over real CLDR data, and this test asserts
/// output in four languages that no English-only fallback could produce.
#[test]
fn formats_in_real_languages_not_english_for_everyone() {
    let c = run(r#"<html><body><div id=t></div><script>
      function f(l, o, v, u) { return new Intl.RelativeTimeFormat(l, o).format(v, u); }
      document.body.setAttribute('data-r', [
        f('en', {}, -3, 'day'),
        f('de', {}, -3, 'day'),
        f('fr', {}, 5, 'month'),
        f('es', { numeric: 'auto' }, -1, 'year')
      ].join(' / '));
    </script></body></html>"#);
    assert_eq!(r(&c), "3 days ago / vor 3 Tagen / dans 5 mois / el año pasado");
}

/// numeric:"auto" selects CLDR's special wording where one exists, which is
/// the whole reason a page asks for this rather than printing a number.
#[test]
fn numeric_auto_uses_the_special_wording() {
    let c = run(r#"<html><body><div id=t></div><script>
      var a = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
      var n = new Intl.RelativeTimeFormat('en', { numeric: 'always' });
      document.body.setAttribute('data-r',
        a.format(-1, 'day') + ' / ' + n.format(-1, 'day') + ' / ' + a.format(1, 'day'));
    </script></body></html>"#);
    assert_eq!(r(&c), "yesterday / 1 day ago / tomorrow");
}

/// Plural category comes from the data, not from a value != 1 test.
#[test]
fn plurals_come_from_the_locale_data() {
    let c = run(r#"<html><body><div id=t></div><script>
      var e = new Intl.RelativeTimeFormat('en');
      var p = new Intl.RelativeTimeFormat('pl');   // Polish has several forms
      document.body.setAttribute('data-r', [
        e.format(-1, 'day'), e.format(-2, 'day'),
        p.format(-1, 'day'), p.format(-2, 'day'), p.format(-5, 'day')
      ].join(' | '));
    </script></body></html>"#);
    let got = r(&c);
    assert!(got.starts_with("1 day ago | 2 days ago | "), "{got}");
    // Polish uses different forms for 2 and 5; an English-only or naive
    // pluraliser cannot produce that distinction.
    let pl: Vec<&str> = got.split(" | ").skip(2).collect();
    assert!(pl[1] != pl[2], "Polish 2 and 5 must differ: {pl:?}");
}

#[test]
fn styles_select_different_patterns() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r', [
        new Intl.RelativeTimeFormat('en', { style: 'long' }).format(-3, 'week'),
        new Intl.RelativeTimeFormat('en', { style: 'short' }).format(-3, 'week'),
        new Intl.RelativeTimeFormat('en', { style: 'narrow' }).format(-3, 'week')
      ].join(' / '));
    </script></body></html>"#);
    let got = r(&c);
    assert!(got.starts_with("3 weeks ago / 3 wk. ago"), "{got}");
}

/// Singular and plural unit names are both accepted, as the spec requires.
#[test]
fn unit_accepts_singular_and_plural() {
    let c = run(r#"<html><body><div id=t></div><script>
      var f = new Intl.RelativeTimeFormat('en');
      document.body.setAttribute('data-r',
        f.format(-3, 'day') + ' / ' + f.format(-3, 'days'));
    </script></body></html>"#);
    assert_eq!(r(&c), "3 days ago / 3 days ago");
}

/// An omitted locale follows the DOCUMENT's language, like every other Intl
/// constructor here — not the host machine's.
#[test]
fn omitted_locale_follows_the_document_language() {
    let c = run(r#"<html lang="de"><body><div id=t></div><script>
      document.body.setAttribute('data-r',
        new Intl.RelativeTimeFormat().format(-3, 'day'));
    </script></body></html>"#);
    assert_eq!(r(&c), "vor 3 Tagen");
}

/// ★ It REFUSES rather than guessing. An unsupported unit throws, so a page
/// that relies on the error keeps its own fallback instead of receiving
/// invented text.
#[test]
fn unsupported_input_throws_rather_than_guessing() {
    let c = run(r#"<html><body><div id=t></div><script>
      var f = new Intl.RelativeTimeFormat('en');
      var a = 'no', b = 'no';
      try { f.format(1, 'fortnight'); } catch (e) { a = e.constructor.name; }
      try { new Intl.RelativeTimeFormat('not-a-locale!!'); } catch (e) { b = 'threw'; }
      document.body.setAttribute('data-r', a + '/' + b);
    </script></body></html>"#);
    assert_eq!(r(&c), "RangeError/threw");
}

/// The feature detection libraries actually use must now answer true, and
/// resolvedOptions must report what was asked for.
#[test]
fn feature_detection_and_resolved_options() {
    let c = run(r#"<html><body><div id=t></div><script>
      var detected = (function () {
        try { return typeof Intl !== 'undefined' && !!Intl.RelativeTimeFormat; }
        catch (e) { return false; }
      })();
      var o = new Intl.RelativeTimeFormat('fr', { style: 'short', numeric: 'auto' })
                .resolvedOptions();
      document.body.setAttribute('data-r',
        detected + '/' + o.locale + '/' + o.style + '/' + o.numeric);
    </script></body></html>"#);
    assert_eq!(r(&c), "true/fr/short/auto");
}
