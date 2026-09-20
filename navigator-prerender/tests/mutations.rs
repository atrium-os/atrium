use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

// ── MutationObserver ────────────────────────────────────────────────────
// The one observer this converter serves for real. These tests assert
// DELIVERY, not registration: the geometry-observer tests assert the
// opposite, and the difference between them is the whole point.

#[test]
fn mutation_observer_delivers_childlist() {
    let c = run(r#"<html><body><div id=t></div><script>
      var seen = 0;
      new MutationObserver(function (recs) { seen += recs.length; })
        .observe(document.getElementById('t'), { childList: true });
      document.getElementById('t').appendChild(document.createElement('p'));
      setTimeout(function () {
        document.body.setAttribute('data-seen', String(seen));
      }, 0);
    </script></body></html>"#);
    assert!(c.html.contains("data-seen=\"1\""), "{}", c.html);
    assert!(c.mutation_records >= 1, "records={}", c.mutation_records);
}

#[test]
fn mutation_observer_respects_subtree() {
    // Without subtree, a mutation on a DESCENDANT must not be delivered.
    let c = run(r#"<html><body><div id=t><span id=s></span></div><script>
      var shallow = 0, deep = 0;
      new MutationObserver(function (r) { shallow += r.length; })
        .observe(document.getElementById('t'), { attributes: true });
      new MutationObserver(function (r) { deep += r.length; })
        .observe(document.getElementById('t'), { attributes: true, subtree: true });
      document.getElementById('s').setAttribute('x', '1');
      setTimeout(function () {
        document.body.setAttribute('data-r', shallow + '/' + deep);
      }, 0);
    </script></body></html>"#);
    assert!(c.html.contains("data-r=\"0/1\""), "{}", c.html);
}

#[test]
fn mutation_observer_filters_and_disconnects() {
    let c = run(r#"<html><body><div id=t></div><script>
      var hits = 0, after = 0;
      var mo = new MutationObserver(function (r) { hits += r.length; });
      mo.observe(document.getElementById('t'),
                 { attributes: true, attributeFilter: ['data-keep'] });
      var t = document.getElementById('t');
      t.setAttribute('data-keep', '1');   // delivered
      t.setAttribute('data-drop', '1');   // filtered out
      setTimeout(function () {
        mo.disconnect();
        t.setAttribute('data-keep', '2'); // after disconnect: nothing
        after = hits;
        document.body.setAttribute('data-r', hits + '/' + after);
      }, 0);
    </script></body></html>"#);
    assert!(c.html.contains("data-r=\"1/1\""), "{}", c.html);
}

#[test]
fn mutation_observer_on_document_observes_the_tree() {
    // `document` is not a node object here; documentElement stands in for it.
    let c = run(r#"<html><body><div id=t></div><script>
      var n = 0;
      new MutationObserver(function (r) { n += r.length; })
        .observe(document, { childList: true, subtree: true });
      document.getElementById('t').appendChild(document.createElement('i'));
      setTimeout(function () { document.body.setAttribute('data-n', String(n)); }, 0);
    </script></body></html>"#);
    assert!(c.html.contains("data-n=\"1\""), "{}", c.html);
}

#[test]
fn mutation_observer_self_feeding_callback_terminates() {
    // A callback that mutates generates records that would re-invoke it. A
    // browser is bounded by the microtask checkpoint; we are bounded by a
    // round count. Either way this must not hang.
    let c = run(r#"<html><body><div id=t></div><script>
      var n = 0;
      var mo = new MutationObserver(function () {
        n++;
        document.getElementById('t').appendChild(document.createElement('b'));
      });
      mo.observe(document.getElementById('t'), { childList: true });
      document.getElementById('t').appendChild(document.createElement('b'));
      setTimeout(function () { document.body.setAttribute('data-n', String(n)); }, 0);
    </script></body></html>"#);
    assert!(c.html.contains("data-n="), "{}", c.html);
    assert!(c.mutation_records < 200, "unbounded: {}", c.mutation_records);
}

#[test]
fn mutation_records_omit_node_lists_honestly() {
    // ★ Recorded limit, asserted so it cannot silently change: the arena does
    // not retain added/removed node lists or old values, so those fields are
    // absent rather than fabricated.
    let c = run(r#"<html><body><div id=t></div><script>
      var kind = '';
      new MutationObserver(function (r) {
        kind = r[0].type + ':' + (r[0].addedNodes === undefined ? 'none' : 'some');
      }).observe(document.getElementById('t'), { childList: true });
      document.getElementById('t').appendChild(document.createElement('p'));
      setTimeout(function () { document.body.setAttribute('data-k', kind); }, 0);
    </script></body></html>"#);
    assert!(c.html.contains("data-k=\"childList:none\""), "{}", c.html);
}
