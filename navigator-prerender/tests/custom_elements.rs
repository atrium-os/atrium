use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// The commonest corpus use by far is the type TEST, not the constructor.
/// Answered from the arena's own nodeType rather than by faking a prototype
/// chain onto the host wrappers — a second notion of "is an element" would
/// be free to disagree with the first.
#[test]
fn instanceof_answers_from_the_arena() {
    let c = run(r#"<html><body><div id=t>x</div><script>
      var d = document.getElementById('t');
      document.body.setAttribute('data-r', [
        d instanceof HTMLElement,
        d instanceof Element,
        d instanceof Node,
        document.createElement('p') instanceof HTMLElement,
        d.firstChild instanceof HTMLElement,     // a text node is not
        d.firstChild instanceof Node,            // but it is a node
        ({}) instanceof HTMLElement,
        null instanceof HTMLElement
      ].join('/'));
    </script></body></html>"#);
    assert_eq!(r(&c), "true/true/true/true/false/true/false/false");
}

/// `new HTMLElement()` outside an upgrade is a TypeError in a browser, and
/// must be here too — a page feature-detecting on that must see the real
/// answer.
#[test]
fn bare_construction_is_an_illegal_constructor() {
    let c = run(r#"<html><body><div id=t></div><script>
      var m = '';
      try { new HTMLElement(); m = 'constructed'; } catch (e) { m = 'threw'; }
      document.body.setAttribute('data-r', m);
    </script></body></html>"#);
    assert_eq!(r(&c), "threw");
}

/// The point of custom elements for a converter: connectedCallback is where
/// a component RENDERS ITSELF, so upgrading is what produces content.
#[test]
fn upgrade_runs_constructor_and_connected_callback() {
    let c = run(r#"<html><body><my-box id=b></my-box><script>
      class MyBox extends HTMLElement {
        constructor() { super(); this.setAttribute('ctor', '1'); }
        connectedCallback() { this.innerHTML = '<p>rendered</p>'; }
      }
      customElements.define('my-box', MyBox);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(c.ce_upgrades, 1, "one element should upgrade");
    assert!(c.html.contains("<p>rendered</p>"), "component did not render: {}", c.html);
    assert!(c.html.contains(r#"ctor="1""#), "constructor did not run on the element: {}", c.html);
}

/// `super()` must bind `this` to the REAL element, not to a fresh object —
/// that is the whole mechanism, and a component that mutates `this` in its
/// constructor is otherwise writing into the void.
#[test]
fn this_in_the_constructor_is_the_real_element() {
    let c = run(r#"<html><body><x-id id=target></x-id><script>
      var same = 'no';
      class XId extends HTMLElement {
        constructor() { super(); same = (this === document.getElementById('target')) ? 'yes' : 'no'; }
      }
      customElements.define('x-id', XId);
      document.body.setAttribute('data-r', same);
    </script></body></html>"#);
    assert_eq!(r(&c), "yes");
}

/// Elements created AFTER define must upgrade too — a component added by a
/// script or a timer is the normal case, not the exception.
#[test]
fn elements_added_later_are_upgraded() {
    let c = run(r#"<html><body><div id=host></div><script>
      class Late extends HTMLElement {
        connectedCallback() { this.textContent = 'late'; }
      }
      customElements.define('x-late', Late);
      setTimeout(function () {
        document.getElementById('host').appendChild(document.createElement('x-late'));
      }, 0);
    </script></body></html>"#);
    assert_eq!(c.ce_upgrades, 1, "late element never upgraded: {}", c.html);
    assert!(c.html.contains(">late<"), "{}", c.html);
}

/// ★ Upgrading requires CONNECTION. connectedCallback firing on a detached
/// node would be a lie about where the element is, and the tree can now
/// answer reachability, so the honest answer is available.
#[test]
fn detached_elements_are_not_upgraded() {
    let c = run(r#"<html><body><div id=t></div><script>
      class Det extends HTMLElement {
        connectedCallback() { this.setAttribute('fired', '1'); }
      }
      customElements.define('x-det', Det);
      var orphan = document.createElement('x-det');   // never inserted
      document.body.setAttribute('data-r', String(orphan.getAttribute('fired')));
    </script></body></html>"#);
    assert_eq!(r(&c), "null");
    assert_eq!(c.ce_upgrades, 0);
}

/// An element upgrades once, however many times the sweep runs.
#[test]
fn upgrade_is_idempotent() {
    let c = run(r#"<html><body><x-once></x-once><script>
      var n = 0;
      class Once extends HTMLElement { connectedCallback() { n++; } }
      customElements.define('x-once', Once);
      customElements.upgrade(document.body);
      setTimeout(function () { document.body.setAttribute('data-r', String(n)); }, 0);
    </script></body></html>"#);
    assert_eq!(r(&c), "1");
    assert_eq!(c.ce_upgrades, 1);
}

/// observedAttributes reports the state actually present at upgrade, which
/// is what a browser reports — not an invented change.
#[test]
fn observed_attributes_report_initial_state() {
    let c = run(r#"<html><body><x-attr size="big" id=a></x-attr><script>
      var log = [];
      class XAttr extends HTMLElement {
        static get observedAttributes() { return ['size', 'absent']; }
        attributeChangedCallback(n, o, v) { log.push(n + '=' + v + ':' + String(o)); }
      }
      customElements.define('x-attr', XAttr);
      document.body.setAttribute('data-r', log.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "size=big:null");
}

/// The registry is queryable, and defining twice is an error as it is in a
/// browser — a page relying on that to avoid double-registration must get
/// the real behaviour.
#[test]
fn registry_is_queryable_and_define_is_once_only() {
    let c = run(r#"<html><body><div id=t></div><script>
      class A extends HTMLElement {}
      customElements.define('x-a', A);
      var second = 'no';
      try { customElements.define('x-a', A); } catch (e) { second = 'threw'; }
      document.body.setAttribute('data-r', [
        customElements.get('x-a') === A,
        customElements.get('x-missing') === undefined,
        second
      ].join('/'));
    </script></body></html>"#);
    assert_eq!(r(&c), "true/true/threw");
}

/// Methods the class defines must be callable on the upgraded element: ours
/// is a host node the constructor ran against rather than a class instance,
/// so the prototype's methods are copied onto it.
#[test]
fn class_methods_are_callable_on_the_element() {
    let c = run(r#"<html><body><x-m id=m></x-m><script>
      class XM extends HTMLElement {
        greet() { return 'hi'; }
      }
      customElements.define('x-m', XM);
      var el = document.getElementById('m');
      document.body.setAttribute('data-r',
        typeof el.greet + '/' + (typeof el.greet === 'function' ? el.greet() : ''));
    </script></body></html>"#);
    assert_eq!(r(&c), "function/hi");
}

/// A constructor that throws must not take the conversion with it, and must
/// not leave the upgrade machinery pointed at a half-built element.
#[test]
fn a_throwing_constructor_is_contained() {
    let c = run(r#"<html><body><x-bad></x-bad><x-good></x-good><script>
      class Bad extends HTMLElement { constructor() { super(); throw new Error('boom'); } }
      class Good extends HTMLElement { connectedCallback() { this.textContent = 'ok'; } }
      customElements.define('x-bad', Bad);
      customElements.define('x-good', Good);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "a component's throw must not fail the page: {:?}", c.errors);
    assert!(c.html.contains(">ok<"), "the good component must still render: {}", c.html);
}
