use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn run(html: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some("https://example.test/docs/a.html"),
                 &mut BoaEngine::default(), &mut NoNetwork)
}

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ The name mapping is the whole substance: `dataset.fooBar` is the
/// attribute `data-foo-bar`. Getting it backwards reads the WRONG attribute
/// rather than failing, so both directions are pinned.
#[test]
fn names_map_between_camel_case_and_dashes() {
    let c = run(r#"<html><body><div id=t data-foo="1" data-foo-bar="2" data-x-y-z="3"></div><script>
      var d = document.getElementById('t').dataset;
      // String() around each: join() renders undefined as EMPTY, which
      // would hide the difference this test exists to check.
      document.body.setAttribute('data-r',
        [d.foo, d.fooBar, d.xYZ, d.xYz].map(String).join('|'));
    </script></body></html>"#);
    // `data-x-y-z` is xYZ — each dash uppercases the NEXT letter, so all
    // three become capitals. `xYz` is a DIFFERENT property (`data-x-yz`)
    // and is absent. I had this backwards first time.
    assert_eq!(r(&c), "1|2|3|undefined");
}

/// An absent data attribute is undefined, not the empty string — pages
/// branch on exactly that difference.
#[test]
fn absent_is_undefined_not_empty() {
    let c = run(r#"<html><body><div id=t data-present=""></div><script>
      var d = document.getElementById('t').dataset;
      document.body.setAttribute('data-r', [
        typeof d.present, typeof d.absent,
        ('present' in d), ('absent' in d)
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "string|undefined|true|false");
}

/// A write really sets the attribute, so it reaches the artifact.
#[test]
fn writes_reach_the_attribute_and_the_artifact() {
    let c = run(r#"<html><body><div id=t></div><script>
      var t = document.getElementById('t');
      t.dataset.userName = 'ada';
      document.body.setAttribute('data-r', t.getAttribute('data-user-name'));
    </script></body></html>"#);
    assert_eq!(r(&c), "ada");
    assert!(c.html.contains(r#"data-user-name="ada""#), "{}", c.html);
}

/// Live in BOTH directions: a setAttribute elsewhere must be visible through
/// dataset, which a snapshot object would miss.
#[test]
fn reads_are_live_against_setattribute() {
    let c = run(r#"<html><body><div id=t></div><script>
      var t = document.getElementById('t');
      var d = t.dataset;
      var before = d.late;
      t.setAttribute('data-late', 'yes');
      document.body.setAttribute('data-r', String(before) + '|' + d.late);
    </script></body></html>"#);
    assert_eq!(r(&c), "undefined|yes");
}

#[test]
fn delete_removes_the_attribute() {
    let c = run(r#"<html><body><div id=t data-gone="1" data-kept="2"></div><script>
      var t = document.getElementById('t');
      delete t.dataset.gone;
      document.body.setAttribute('data-r',
        String(t.dataset.gone) + '|' + t.dataset.kept + '|' + t.hasAttribute('data-gone'));
    </script></body></html>"#);
    assert_eq!(r(&c), "undefined|2|false");
    // Attribute form: the artifact carries the script source too, which
    // mentions 'data-gone' in the hasAttribute call that reported false.
    assert!(!c.html.contains(r#"data-gone=""#), "{}", c.html);
}

/// Enumeration must see the real set — and only the data-* attributes.
#[test]
fn enumeration_lists_only_data_attributes() {
    let c = run(r#"<html><body><div id=t class=c data-one="1" data-two-part="2"></div><script>
      var d = document.getElementById('t').dataset;
      var keys = Object.keys(d);
      var seen = []; for (var k in d) seen.push(k);
      document.body.setAttribute('data-r',
        keys.sort().join(',') + '|' + seen.sort().join(',') + '|' + keys.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "one,twoPart|one,twoPart|2");
}

/// dataset writes are real mutations, so MutationObserver must see them
/// under the mapped attribute name.
#[test]
fn dataset_writes_reach_mutation_observers() {
    let c = run(r#"<html><body><div id=t></div><script>
      var names = [];
      new MutationObserver(function (recs) {
        for (var i = 0; i < recs.length; i++) names.push(recs[i].attributeName);
      }).observe(document.getElementById('t'), { attributes: true });
      document.getElementById('t').dataset.someKey = 'v';
      setTimeout(function () { document.body.setAttribute('data-r', names.join(',')); }, 0);
    </script></body></html>"#);
    assert_eq!(r(&c), "data-some-key");
}

/// `document.location` IS `location` — and it must track history, not be a
/// copy taken at startup.
#[test]
fn document_location_is_location_and_tracks_history() {
    let c = run(r#"<html><body><div id=t></div><script>
      var same = (document.location === location) && (document.location === window.location);
      var before = document.location.pathname;
      history.pushState({}, '', 'moved.html');
      document.body.setAttribute('data-r',
        same + '|' + before + '|' + document.location.pathname + '|' + document.URL);
    </script></body></html>"#);
    assert_eq!(r(&c),
        "true|/docs/a.html|/docs/moved.html|https://example.test/docs/moved.html");
}

/// Assigning document.location is navigation, which this converter must
/// never perform — it is accepted and ignored, not obeyed.
#[test]
fn assigning_document_location_does_not_navigate() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.location = 'https://evil.test/';
      document.body.setAttribute('data-r', document.location.host);
    </script></body></html>"#);
    assert_eq!(r(&c), "example.test");
}
