use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::MapFetcher};

fn run(html: &str) -> navigator_prerender::Conversion {
    let c = convert_with(html, Some("https://example.test/p.html"), &mut BoaEngine::default(), &mut MapFetcher::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    c
}

/// HTML's event handler content attributes: `onclick="..."` is the
/// element's handler, `this` is the element, `event` the event; script
/// reads it back as a function, a later attribute text replaces it, and a
/// slot assigned from script wins.
#[test]
fn an_onclick_attribute_is_the_elements_handler() {
    let c = run(r#"<html><body>
      <button id=a onclick="this.setAttribute('ran', event.type)">A</button>
      <button id=b onclick="this.setAttribute('ran', 'old')">B</button>
      <button id=c onclick="this.setAttribute('ran', 'attr')">C</button>
      <button id=d onclick="this is not javascript">D</button>
      <script>
        var a = document.getElementById('a'), b = document.getElementById('b'),
            c = document.getElementById('c'), d = document.getElementById('d');
        function press(x) { x.dispatchEvent(new MouseEvent('click', { bubbles: true })); }
        document.body.setAttribute('kind', typeof a.onclick);
        press(a);
        b.setAttribute('onclick', "this.setAttribute('ran', 'new')");
        press(b);
        c.onclick = function () { c.setAttribute('ran', 'slot'); };
        press(c);
        press(d);
        document.body.setAttribute('after', 'yes');
      </script></body></html>"#);
    assert!(c.html.contains(r#"kind="function""#), "{}", c.html);
    assert!(c.html.contains(r#"id="a" onclick="this.setAttribute('ran', event.type)" ran="click""#), "{}", c.html);
    assert!(c.html.contains(r#"ran="new""#) && !c.html.contains(r#"ran="old""#), "{}", c.html);
    assert!(c.html.contains(r#"ran="slot""#) && !c.html.contains(r#"ran="attr""#), "{}", c.html);
    assert!(c.html.contains(r#"after="yes""#), "a handler that does not compile took the page with it: {}", c.html);
}

/// ★ A consent wall wired only by attributes is found (legacy-web §5.4.1e).
#[test]
fn a_wall_wired_by_attributes_is_found() {
    let c = run(r#"<html><body><div role=dialog>
      <button onclick="document.cookie = 'ok=1; Path=/'">Accept</button>
      <button onclick="document.cookie = 'ok=0; Path=/'">Reject</button></div></body></html>"#);
    assert_eq!(c.consents, vec![vec!["ok=1; Path=/".to_string()], vec!["ok=0; Path=/".to_string()]]);
    assert!(c.html.contains(r#"<a href="atrium-consent:0">Accept</a>"#), "{}", c.html);
}

/// `click()` dispatches a bubbling click (not on a disabled control);
/// `focus()` and `blur()` move `document.activeElement` with focus and
/// blur events, in a browser's order.
#[test]
fn click_focus_and_blur_do_what_a_browser_does() {
    let c = run(r#"<html><body><div id=box>
      <button id=a onclick="this.setAttribute('ran', '1')">A</button>
      <button id=off disabled onclick="this.setAttribute('ran', '1')">Off</button>
      <input id=x><input id=y></div>
      <script>
        var log = [];
        var box = document.getElementById('box');
        box.addEventListener('click', function (e) { log.push('click:' + e.target.id + ':' + e.isTrusted); });
        ['focusin', 'focusout'].forEach(function (t) { box.addEventListener(t, function (e) { log.push(t + ':' + e.target.id); }); });
        document.getElementById('a').click();
        document.getElementById('off').click();
        var x = document.getElementById('x'), y = document.getElementById('y');
        log.push('active:' + document.activeElement.tagName);
        x.addEventListener('blur', function () { log.push('blur:x'); });
        x.focus(); y.focus(); y.blur();
        log.push('active:' + document.activeElement.tagName);
        document.body.setAttribute('log', log.join(' '));
      </script></body></html>"#);
    assert!(c.html.contains(r#"id="a" onclick="this.setAttribute('ran', '1')" ran="1""#), "{}", c.html);
    assert!(!c.html.contains(r#"disabled="" onclick="this.setAttribute('ran', '1')" ran"#), "{}", c.html);
    assert!(c.html.contains(r#"log="click:a:false active:BODY focusin:x blur:x focusout:x focusin:y focusout:y active:BODY""#), "{}", c.html);
}
