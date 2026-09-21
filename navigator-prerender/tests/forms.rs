use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// value means a different thing per control: an attribute for input, the
/// CONTENT for textarea, and the selected option for select.
#[test]
fn value_reads_the_right_thing_per_control() {
    let c = run(r#"<html><body>
      <input id=i value="typed">
      <textarea id=a>inner text</textarea>
      <select id=s><option value=one>One</option><option value=two selected>Two</option></select>
      <select id=s2><option value=first>First</option><option value=second>Second</option></select>
      <script>
        document.body.setAttribute('data-r', [
          document.getElementById('i').value,
          document.getElementById('a').value,
          document.getElementById('s').value,
          document.getElementById('s2').value
        ].join('|'));
      </script></body></html>"#);
    // An untouched select reports its first option, as a browser does.
    assert_eq!(r(&c), "typed|inner text|two|first");
}

/// ★ A deliberate divergence, asserted so it cannot drift: a browser keeps an
/// assigned value as separate "dirty" state and does NOT write the attribute,
/// so its own serialization loses it. This converter WRITES the attribute,
/// because the artifact is what a reader sees and a field a script filled in
/// should still be filled in when they read it.
#[test]
fn an_assigned_value_reaches_the_artifact() {
    let c = run(r#"<html><body><input id=i value="old"><script>
      var i = document.getElementById('i');
      i.value = 'filled by script';
      document.body.setAttribute('data-r', i.value + '|' + i.getAttribute('value'));
    </script></body></html>"#);
    assert_eq!(r(&c), "filled by script|filled by script");
    assert!(c.html.contains(r#"value="filled by script""#), "{}", c.html);
}

#[test]
fn textarea_value_writes_its_content() {
    let c = run(r#"<html><body><textarea id=a>old</textarea><script>
      var a = document.getElementById('a');
      a.value = 'new body';
      document.body.setAttribute('data-r', a.value + '|' + a.textContent);
    </script></body></html>"#);
    assert_eq!(r(&c), "new body|new body");
    assert!(c.html.contains("<textarea id=\"a\">new body</textarea>"), "{}", c.html);
}

/// ★ SVG and MathML names keep their CASE. Lowercasing would produce
/// elements no renderer recognises, in a document the converter serializes.
#[test]
fn create_element_ns_preserves_case_and_namespace() {
    let c = run(r#"<html><body><div id=t></div><script>
      var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
      var grad = document.createElementNS('http://www.w3.org/2000/svg', 'linearGradient');
      grad.setAttribute('id', 'g1');
      svg.appendChild(grad);
      document.getElementById('t').appendChild(svg);
      document.body.setAttribute('data-r',
        grad.tagName + '|' + grad.namespaceURI + '|' + svg.children.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "LINEARGRADIENT|http://www.w3.org/2000/svg|1");
    // The SERIALIZED name must keep its camelCase.
    assert!(c.html.contains("<linearGradient id=\"g1\">"), "case was lost: {}", c.html);
}

/// ★ A list of nodes must not say "[object Array]". Libraries validate with
/// exactly this check and THROW when it matches neither NodeList nor
/// HTMLCollection.
#[test]
fn node_lists_report_their_own_types() {
    let c = run(r#"<html><body><div id=t><p>a</p><p>b</p></div><script>
      var s = Object.prototype.toString;
      var t = document.getElementById('t');
      document.body.setAttribute('data-r', [
        s.call(document.querySelectorAll('p')),
        s.call(t.childNodes),
        s.call(t.children),
        s.call(document.getElementsByTagName('p'))
      ].join(' '));
    </script></body></html>"#);
    assert_eq!(r(&c),
        "[object NodeList] [object NodeList] [object HTMLCollection] [object HTMLCollection]");
}

/// The validation idiom from the corpus must now pass rather than throw.
#[test]
fn the_corpus_type_validation_idiom_passes() {
    let c = run(r#"<html><body><div id=t><p>a</p></div><script>
      function check(x) {
        var n = Object.prototype.toString.call(x);
        if (n !== '[object NodeList]' && n !== '[object HTMLCollection]') {
          throw new TypeError('String, HTMLElement, HTMLCollection, or NodeList');
        }
        return 'accepted';
      }
      var a = check(document.querySelectorAll('p'));
      var b = NodeList.prototype.isPrototypeOf(document.querySelectorAll('p'));
      document.body.setAttribute('data-r', a + '|' + b);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(r(&c), "accepted|true");
}

/// Reparenting must not cost the array methods pages rely on.
#[test]
fn lists_keep_array_methods_and_iteration() {
    let c = run(r#"<html><body><div id=t><p>a</p><p>b</p><p>c</p></div><script>
      var l = document.querySelectorAll('p');
      var viaForEach = [];
      l.forEach(function (n) { viaForEach.push(n.textContent); });
      var viaSpread = [...l].map(function (n) { return n.textContent; });
      document.body.setAttribute('data-r',
        l.length + '|' + viaForEach.join('') + '|' + viaSpread.join('') + '|' +
        l.item(1).textContent + '|' + String(l.item(9)));
    </script></body></html>"#);
    assert_eq!(r(&c), "3|abc|abc|b|null");
}

/// ★ `new Image()` never fetches. The corpus uses it almost entirely as a
/// tracking pixel; this converter loads no images, so the request simply does
/// not happen — the same outcome the network policy reaches deliberately.
#[test]
fn image_is_an_img_element_that_never_fetches() {
    let c = run(r#"<html><body><div id=t></div><script>
      var img = new Image(1, 1);
      img.src = 'https://tracker.test/pixel.gif';
      img.alt = '';
      document.body.setAttribute('data-r',
        img.tagName + '|' + img.getAttribute('width') + '|' + img.getAttribute('height'));
    </script></body></html>"#);
    assert_eq!(r(&c), "IMG|1|1");
    assert_eq!(c.page_fetches, 0, "an Image must never fetch");
    assert_eq!(c.page_blocked, 0, "and it is not even a refusal — no request is made");
}
