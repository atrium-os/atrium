use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// A comment node is REAL — navigable, typed, positioned — even though the
/// serializer drops comments, as it already does for those in the source.
/// Frameworks use them as placeholders to position against.
#[test]
fn create_comment_makes_a_real_navigable_node() {
    let c = run(r#"<html><body><div id=t><i>a</i></div><script>
      var t = document.getElementById('t');
      var m = document.createComment('anchor');
      t.insertBefore(m, t.firstChild);
      document.body.setAttribute('data-r', [
        m.nodeType, m.nodeValue, m.nodeName,
        t.childNodes.length, (t.firstChild === m),
        (m.nextSibling.textContent)
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "8|anchor|#comment|2|true|a");
}

/// ★ Recorded limit, asserted so it cannot drift: the comment does NOT reach
/// the artifact. The content positioned against it does.
#[test]
fn comments_are_navigable_but_not_serialized() {
    let c = run(r#"<html><body><div id=t></div><script>
      var t = document.getElementById('t');
      t.appendChild(document.createComment('MARKER'));
      var p = document.createElement('p');
      p.textContent = 'after';
      t.appendChild(p);
    </script></body></html>"#);
    let after = &c.html[c.html.find("</script>").unwrap()..];
    assert!(!after.contains("MARKER"), "comment leaked into the artifact: {}", c.html);
    assert!(c.html.contains("<p>after</p>"), "{}", c.html);
}

#[test]
fn document_scripts_is_live() {
    let c = run(r#"<html><body><div id=t></div><script>
      var before = document.scripts.length;
      document.body.appendChild(document.createElement('script'));
      document.body.setAttribute('data-r',
        before + '|' + document.scripts.length + '|' + document.scripts[0].tagName);
    </script></body></html>"#);
    assert_eq!(r(&c), "1|2|SCRIPT");
}

#[test]
fn default_view_is_the_window() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r',
        (document.defaultView === window) + '|' +
        (document.defaultView.document === document));
    </script></body></html>"#);
    assert_eq!(r(&c), "true|true");
}

/// ★ The document reports itself VISIBLE. Neither answer is observed fact —
/// there is no viewport — but they are not symmetric: a page told it is
/// hidden defers exactly the work a converter exists to capture.
#[test]
fn document_reports_itself_visible() {
    let c = run(r#"<html><body><div id=t></div><script>
      var built = 'no';
      if (!document.hidden && document.visibilityState === 'visible') {
        built = 'yes';
      }
      document.body.setAttribute('data-r',
        document.hidden + '|' + document.visibilityState + '|' + built);
    </script></body></html>"#);
    assert_eq!(r(&c), "false|visible|yes");
}

/// TreeWalker in document order, filtered by whatToShow. The bitmask is over
/// (nodeType - 1), which is easy to get subtly wrong.
#[test]
fn tree_walker_visits_in_document_order() {
    let c = run(r#"<html><body><div id=t><p>one</p><span><b>two</b></span><p>three</p></div><script>
      var w = document.createTreeWalker(document.getElementById('t'), NodeFilter.SHOW_ELEMENT);
      var seen = [], n;
      while ((n = w.nextNode())) seen.push(n.tagName);
      document.body.setAttribute('data-r', seen.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "P,SPAN,B,P");
}

#[test]
fn tree_walker_show_text_collects_text_nodes() {
    let c = run(r#"<html><body><div id=t><p>one</p><span>two</span></div><script>
      var w = document.createTreeWalker(document.getElementById('t'), NodeFilter.SHOW_TEXT);
      var out = [], n;
      while ((n = w.nextNode())) out.push(n.nodeValue);
      document.body.setAttribute('data-r', out.join('+') + '|' + out.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "one+two|2");
}

/// FILTER_REJECT skips a whole subtree; FILTER_SKIP skips only the node.
#[test]
fn reject_skips_the_subtree_skip_skips_one_node() {
    let c = run(r#"<html><body><div id=t>
        <div class=drop><b>hidden</b></div><div class=keep><i>kept</i></div>
      </div><script>
      function walk(mode) {
        var w = document.createTreeWalker(document.getElementById('t'),
          NodeFilter.SHOW_ELEMENT, function (n) {
            if (n.getAttribute('class') === 'drop') return mode;
            return NodeFilter.FILTER_ACCEPT;
          });
        var seen = [], n;
        while ((n = w.nextNode())) seen.push(n.tagName);
        return seen.join(',');
      }
      document.body.setAttribute('data-r',
        walk(NodeFilter.FILTER_REJECT) + ' || ' + walk(NodeFilter.FILTER_SKIP));
    </script></body></html>"#);
    // REJECT drops the DIV and its <b>; SKIP drops only the DIV.
    assert_eq!(r(&c), "DIV,I || B,DIV,I");
}

#[test]
fn tree_walker_navigates_relatives() {
    let c = run(r#"<html><body><div id=t><p id=a>a</p><p id=b>b</p><p id=c>c</p></div><script>
      var root = document.getElementById('t');
      var w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
      var first = w.firstChild().getAttribute('id');
      var next = w.nextSibling().getAttribute('id');
      var prev = w.previousSibling().getAttribute('id');
      var up = (w.parentNode() === root);
      document.body.setAttribute('data-r', [first, next, prev, up].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "a|b|a|true");
}

/// A walker reflects mutations made between steps — the reason a page uses
/// one instead of collecting an array up front.
#[test]
fn tree_walker_sees_mutations_between_steps() {
    let c = run(r#"<html><body><div id=t><p>one</p></div><script>
      var root = document.getElementById('t');
      var w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
      var a = w.nextNode().tagName;
      var added = document.createElement('em');
      root.appendChild(added);
      var b = w.nextNode();
      document.body.setAttribute('data-r', a + '|' + (b ? b.tagName : 'null'));
    </script></body></html>"#);
    assert_eq!(r(&c), "P|EM");
}

/// previousNode walks back in document order, descending into the deepest
/// last descendant of the previous sibling — the mirror of nextNode, and the
/// half most often got wrong.
#[test]
fn tree_walker_previous_node_mirrors_next() {
    let c = run(r#"<html><body><div id=t><p>a</p><span><b>b</b></span><p>c</p></div><script>
      var w = document.createTreeWalker(document.getElementById('t'), NodeFilter.SHOW_ELEMENT);
      var fwd = [], n;
      while ((n = w.nextNode())) fwd.push(n.tagName);
      var back = [];
      while ((n = w.previousNode())) back.push(n.tagName);
      document.body.setAttribute('data-r', fwd.join(',') + ' || ' + back.join(','));
    </script></body></html>"#);
    // Walking back from the last node must retrace the same path.
    assert_eq!(r(&c), "P,SPAN,B,P || B,SPAN,P");
}
