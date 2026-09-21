use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

/// Read `data-r` back out of the artifact, so each test asserts on what the
/// CONVERSION produced rather than on engine internals.
fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ NODE IDENTITY. Wrappers used to be minted per lookup, so two reads of
/// the same node compared unequal and `window.document === document` was
/// false. jQuery's setDocument opens with `doc == document`; libraries key
/// caches on nodes and walk parents comparing against a root. Identity is
/// part of the contract.
#[test]
fn node_identity_is_stable() {
    let c = run(r#"<html><body><div id=t></div><script>
      var a = document.getElementById('t'), b = document.getElementById('t');
      document.body.setAttribute('data-r',
        (a === b) + '/' + (document.body === document.body) + '/' +
        (window.document === document) + '/' +
        (document.getElementById('t').parentNode === document.body));
    </script></body></html>"#);
    assert_eq!(r(&c), "true/true/true/true");
}

/// document must report nodeType 9. This single missing property was what
/// stopped jQuery: Sizzle's setDocument bails on `doc.nodeType !== 9` and
/// leaves its own `document` undefined forever.
#[test]
fn node_types_are_reported() {
    let c = run(r#"<html><body><div id=t>x</div><script>
      var d = document.getElementById('t');
      document.body.setAttribute('data-r', document.nodeType + '/' + d.nodeType
        + '/' + d.firstChild.nodeType + '/' + document.createDocumentFragment().nodeType
        + '/' + d.nodeName + '/' + document.nodeName);
    </script></body></html>"#);
    assert_eq!(r(&c), "9/1/3/11/DIV/#document");
}

#[test]
fn navigation_reads_the_live_tree() {
    let c = run(r#"<html><body><ul id=t><li>a</li><li>b</li><li>c</li></ul><script>
      var u = document.getElementById('t');
      var out = [u.childNodes.length, u.children.length,
                 u.firstChild.textContent, u.lastChild.textContent,
                 u.firstElementChild.nextSibling.textContent,
                 u.lastElementChild.previousSibling.textContent,
                 u.hasChildNodes(), u.contains(u.firstChild)];
      document.body.setAttribute('data-r', out.join('/'));
    </script></body></html>"#);
    assert_eq!(r(&c), "3/3/a/c/b/b/true/true");
}

/// A snapshot taken when the wrapper was built would be stale the moment
/// anything moved, so these are accessors over the arena, not stored values.
#[test]
fn navigation_is_live_not_snapshotted() {
    let c = run(r#"<html><body><ul id=t><li>a</li></ul><script>
      var u = document.getElementById('t');
      var before = u.childNodes.length;
      u.appendChild(document.createElement('li'));
      var after = u.childNodes.length;
      u.removeChild(u.lastChild);
      document.body.setAttribute('data-r', before + '/' + after + '/' + u.childNodes.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "1/2/1");
}

#[test]
fn insert_before_and_replace_and_remove() {
    let c = run(r#"<html><body><ul id=t><li id=x>x</li></ul><script>
      var u = document.getElementById('t'), x = document.getElementById('x');
      var a = document.createElement('li'); a.textContent = 'a';
      u.insertBefore(a, x);                       // a before x
      var b = document.createElement('li'); b.textContent = 'b';
      u.insertBefore(b, null);                    // null ref => append
      var z = document.createElement('li'); z.textContent = 'z';
      u.replaceChild(z, x);                       // z replaces x
      document.body.setAttribute('data-r', u.textContent + '/' + u.children.length);
      b.remove();
      document.body.setAttribute('data-r2', u.textContent);
    </script></body></html>"#);
    assert_eq!(r(&c), "azb/3");
    assert!(c.html.contains("data-r2=\"az\""), "{}", c.html);
}

/// ★ Deep clone must actually copy the subtree. jQuery reads
/// `div.cloneNode(true).cloneNode(true).lastChild.checked` — a clone that
/// drops children throws on the very next property access.
#[test]
fn clone_node_deep_and_shallow() {
    let c = run(r#"<html><body><div id=t><p>kid</p></div><script>
      var t = document.getElementById('t');
      var deep = t.cloneNode(true), flat = t.cloneNode(false);
      var chain = t.cloneNode(true).cloneNode(true);
      document.body.setAttribute('data-r',
        deep.childNodes.length + '/' + flat.childNodes.length + '/' +
        chain.lastChild.textContent + '/' + (deep.parentNode === null) + '/' +
        (deep === t));
    </script></body></html>"#);
    assert_eq!(r(&c), "1/0/kid/true/false");
}

/// A fragment inserts its CHILDREN, never itself — the behaviour every
/// template-building library depends on.
#[test]
fn fragment_inserts_its_children() {
    let c = run(r#"<html><body><div id=t></div><script>
      var f = document.createDocumentFragment();
      f.appendChild(document.createElement('i'));
      f.appendChild(document.createElement('b'));
      var t = document.getElementById('t');
      t.appendChild(f);
      document.body.setAttribute('data-r',
        t.children.length + '/' + t.firstChild.nodeName + '/' + t.lastChild.nodeName);
    </script></body></html>"#);
    assert_eq!(r(&c), "2/I/B");
    assert!(!c.html.contains("#document-fragment"), "fragment leaked into output");
}

/// innerHTML goes through the SAME html5ever parse as the document. A second,
/// hand-rolled parser would disagree with the one that built the page, which
/// is the divergence this crate exists to avoid.
#[test]
fn inner_html_round_trips_through_the_real_parser() {
    let c = run(r#"<html><body><div id=t></div><script>
      var t = document.getElementById('t');
      t.innerHTML = '<p class="k">one</p><p>two</p>';
      document.body.setAttribute('data-r',
        t.children.length + '/' + t.firstChild.getAttribute('class') + '/' +
        t.textContent + '/' + (t.innerHTML.indexOf('<p class="k">') === 0));
    </script></body></html>"#);
    assert_eq!(r(&c), "2/k/onetwo/true");
    assert!(c.html.contains(r#"<div id="t"><p class="k">one</p><p>two</p></div>"#), "{}", c.html);
}

/// Setting innerHTML replaces what was there; it does not append.
#[test]
fn inner_html_replaces_existing_children() {
    let c = run(r#"<html><body><div id=t><span>old</span></div><script>
      var t = document.getElementById('t');
      t.innerHTML = '<em>new</em>';
      document.body.setAttribute('data-r', t.children.length + '/' + t.textContent);
    </script></body></html>"#);
    assert_eq!(r(&c), "1/new");
    assert!(!c.html.contains("old"), "replaced content survived: {}", c.html);
}

/// Tree mutations are real mutations, so MutationObserver must see them —
/// the two features have to agree or one of them is lying.
#[test]
fn tree_mutations_reach_mutation_observers() {
    let c = run(r#"<html><body><div id=t><i>a</i></div><script>
      var n = 0;
      new MutationObserver(function (recs) { n += recs.length; })
        .observe(document.getElementById('t'), { childList: true });
      var t = document.getElementById('t');
      t.insertBefore(document.createElement('b'), t.firstChild);
      t.removeChild(t.lastChild);
      t.innerHTML = '<u>u</u>';
      setTimeout(function () { document.body.setAttribute('data-r', String(n)); }, 0);
    </script></body></html>"#);
    assert_eq!(r(&c), "3");
}

/// jQuery parses untrusted markup in a detached document. The nodes it makes
/// there are real, and genuinely cannot reach the page on their own.
#[test]
fn create_html_document_is_detached_and_real() {
    let c = run(r#"<html><body><div id=t></div><script>
      var d = document.implementation.createHTMLDocument('');
      d.body.innerHTML = '<form></form><form></form>';
      document.body.setAttribute('data-r',
        d.body.childNodes.length + '/' + d.nodeType + '/' + (d === document));
    </script></body></html>"#);
    assert_eq!(r(&c), "2/9/false");
    // Scoped past the script: the markup string is in the source that built
    // the detached document, and script text serializes verbatim.
    let body = c.html.rsplit_once("</script>").map(|(_, t)| t).unwrap_or(&c.html);
    assert!(!body.contains("<form>"), "detached document leaked into the artifact: {}", c.html);
}

/// A cycle would make serialization non-terminating, so the arena refuses it.
#[test]
fn appending_an_ancestor_into_its_own_descendant_is_refused() {
    let c = run(r#"<html><body><div id=a><div id=b></div></div><script>
      var a = document.getElementById('a'), b = document.getElementById('b');
      b.appendChild(a);            // would make a cycle
      document.body.setAttribute('data-r',
        (a.parentNode === document.body) + '/' + (b.parentNode === a));
    </script></body></html>"#);
    assert_eq!(r(&c), "true/true");
}
