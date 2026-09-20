use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ A NamedNodeMap is addressed THREE ways and a plain array serves only
/// one. The corpus reads `attrs.length` with `attrs[i]`, AND `attrs[name]`,
/// AND `attrs.placeholder` as a bare property — all three across its 713
/// references. Each is pinned here.
#[test]
fn addressed_by_index_by_name_and_as_a_property() {
    let c = run(r#"<html><body><input id=t placeholder="type here" data-k="v"><script>
      var a = document.getElementById('t').attributes;
      document.body.setAttribute('data-r', [
        a.length,
        a[0].name,
        a['placeholder'].value,
        a.placeholder.nodeValue,
        a.getNamedItem('data-k').value
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "3|id|type here|type here|v");
}

/// The iteration idiom the corpus actually uses: length plus destructured
/// {name, value} per index.
#[test]
fn the_length_and_index_iteration_idiom() {
    let c = run(r#"<html><body><div id=t a="1" b="2" c="3"></div><script>
      var e = document.getElementById('t');
      var out = [];
      if (e.hasAttributes()) {
        var n = e.attributes.length;
        for (var i = 0; i < n; i++) {
          var at = e.attributes[i];
          out.push(at.name + '=' + at.value);
        }
      }
      document.body.setAttribute('data-r', out.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "id=t,a=1,b=2,c=3");
}

/// An absent attribute is undefined by property access and null by
/// getNamedItem — the two differ in the DOM and pages branch on both.
#[test]
fn absent_is_undefined_by_property_and_null_by_named_item() {
    let c = run(r#"<html><body><div id=t></div><script>
      var a = document.getElementById('t').attributes;
      document.body.setAttribute('data-r', [
        typeof a.nope, String(a.getNamedItem('nope')),
        ('id' in a), ('nope' in a), String(a.item(9))
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "undefined|null|true|false|null");
}

/// Live against the arena: an attribute set elsewhere is visible, and one
/// removed through the map really goes.
#[test]
fn the_map_is_live_in_both_directions() {
    let c = run(r#"<html><body><div id=t gone="1"></div><script>
      var e = document.getElementById('t');
      var before = e.attributes.length;
      e.setAttribute('added', 'yes');
      var mid = e.attributes.length + '/' + e.attributes.added.value;
      e.attributes.removeNamedItem('gone');
      document.body.setAttribute('data-r',
        before + '|' + mid + '|' + e.attributes.length + '|' + e.hasAttribute('gone'));
    </script></body></html>"#);
    assert_eq!(r(&c), "2|3/yes|2|false");
    let after = &c.html[c.html.find("</script>").unwrap()..];
    assert!(!after.contains("gone="), "removed attribute survived: {}", c.html);
}

/// Enumeration yields INDICES, as a browser does for a NamedNodeMap — so
/// Array.from and the spread both produce the Attr list.
#[test]
fn enumeration_yields_indices_so_array_from_works() {
    let c = run(r#"<html><body><div id=t p="1" q="2"></div><script>
      var a = document.getElementById('t').attributes;
      var keys = Object.keys(a);
      var names = Array.from(a).map(function (x) { return x.name; });
      document.body.setAttribute('data-r', keys.join(',') + '|' + names.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "0,1,2|id,p,q");
}

/// Attr carries its owner, which delegation code walks back through.
#[test]
fn attr_knows_its_owner_element() {
    let c = run(r#"<html><body><div id=t k="v"></div><script>
      var e = document.getElementById('t');
      document.body.setAttribute('data-r',
        String(e.attributes.k.ownerElement === e) + '|' + e.attributes.k.specified);
    </script></body></html>"#);
    assert_eq!(r(&c), "true|true");
}

/// hasAttributes reports emptiness, and an element with none has a
/// zero-length map rather than a missing one.
#[test]
fn an_element_without_attributes_has_an_empty_map() {
    let c = run(r#"<html><body><div id=t></div><script>
      var bare = document.createElement('span');
      document.body.setAttribute('data-r',
        bare.hasAttributes() + '|' + bare.attributes.length + '|' +
        document.getElementById('t').hasAttributes());
    </script></body></html>"#);
    assert_eq!(r(&c), "false|0|true");
}
