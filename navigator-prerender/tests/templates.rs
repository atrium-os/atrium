//! Sibling generalisation (legacy-web §5.4.5.6c): a handler's transition,
//! learned on the elements it was probed on, recorded ONCE as a template over
//! the elements that share it, and verified by probing a sample of the
//! unprobed ones.

use navigator_prerender::{artifact, boa_impl::BoaEngine, convert_with, fetch::NoNetwork, Conversion,
    Effect, TierPolicy, Transition};

fn explore_with(html: &str, bounds: Option<(usize, usize)>) -> Conversion {
    let mut eng = BoaEngine { explore: true, probe_bounds: bounds, ..Default::default() };
    convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork)
}
fn plain(html: &str) -> Conversion {
    convert_with(html, Some("https://example.test/p.html"), &mut BoaEngine::default(), &mut NoNetwork)
}
/// The ORACLE: every element probed, no bound reached.
fn unbounded(html: &str) -> Conversion { explore_with(html, Some((100_000, 100_000))) }

fn sorted(mut v: Vec<Transition>) -> Vec<(String, String, bool, Vec<Effect>)> {
    let mut o: Vec<_> = v.drain(..).map(|t| (t.trigger, t.event, t.anchored, t.effects)).collect();
    o.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    o
}

/// `n` tabs in a list and `n` panels beside it; tab i shows panel i. No ids:
/// the addresses are positional, so the template has to learn "the panel at
/// the tab's own index".
fn tabs(n: usize) -> String {
    let mut h = String::from("<html><body><ul>");
    for _ in 0..n { h.push_str("<li><a href=#>t</a></li>"); }
    h.push_str("</ul><div>");
    for _ in 0..n { h.push_str("<div class=panel>p</div>"); }
    h.push_str("</div><script>var as = document.getElementsByTagName('a');\n\
        var ps = document.getElementsByClassName('panel');\n\
        for (var i = 0; i < as.length; i++) (function (i) { as[i].addEventListener('click', function () {\n\
          ps[i].setAttribute('class', 'panel shown'); }); })(i);\n</script></body></html>");
    h
}

/// ★★ The tab list: 64 probes, 3 verification samples, the other 133
/// predicted, and the expansion is EXACTLY what probing every tab records.
#[test]
fn tab_i_shows_panel_i_is_one_template() {
    let html = tabs(200);
    let c = explore_with(&html, None);
    assert_eq!(c.templates.len(), 1, "{:?}", c.templates);
    let t = &c.templates[0];
    assert_eq!(t.instances.len(), 200);
    assert_eq!(t.effects.len(), 1);
    assert!(matches!(&t.effects[0], Effect::Attribute { target, .. } if target == "$0"), "{:?}", t.effects);
    assert!(c.transitions.is_empty(), "the examples fold into the template: {:?}", c.transitions);
    let s = &c.explore_stats;
    assert_eq!((s.probes, s.verify_probes, s.predicted, s.refused), (64, 3, 133, 0), "{s:?}");
    assert_eq!(sorted(c.expanded_transitions()), sorted(unbounded(&html).expanded_transitions()),
        "the template predicts what probing every tab records");
    // Coverage: the predicted ones say so, the samples say so.
    assert_eq!(c.coverage.iter().filter(|k| k.templated).count(), 133);
    assert_eq!(c.coverage.iter().filter(|k| k.sampled).count(), 3);
    assert!(c.coverage.iter().all(|k| !k.represented), "nothing is left merely represented");
    assert_eq!(c.html, plain(&html).html, "templates change the recording, never the artifact");
}

/// An accordion: each header toggles ITS OWN body, the next sibling.
#[test]
fn an_item_toggling_its_own_body_is_one_template() {
    let mut h = String::from("<html><body>");
    for _ in 0..90 { h.push_str("<section><h3>q</h3><div aria-hidden=true>a</div></section>"); }
    h.push_str("<script>var hs = document.getElementsByTagName('h3');\n\
        for (var i = 0; i < hs.length; i++) hs[i].addEventListener('click', function () {\n\
          this.parentNode.getElementsByTagName('div')[0].setAttribute('aria-hidden', 'false');\n\
          this.setAttribute('aria-expanded', 'true'); });\n</script></body></html>");
    let c = explore_with(&h, None);
    assert_eq!(c.templates.len(), 1, "{:?}", c.templates);
    assert_eq!(c.templates[0].instances.len(), 90);
    assert_eq!(sorted(c.expanded_transitions()), sorted(unbounded(&h).expanded_transitions()));
}

/// The commonest case: a class toggle on the element itself is `$t`, and
/// needs no slot at all.
#[test]
fn a_toggle_on_itself_is_addressed_as_the_trigger() {
    let mut h = String::from("<html><body>");
    for i in 0..80 { h.push_str(&format!("<button id=b{i} class=x>b</button>")); }
    h.push_str("<script>var bs = document.getElementsByTagName('button');\n\
        for (var i = 0; i < bs.length; i++) bs[i].addEventListener('click', function () { this.className = 'x on'; });\n\
        </script></body></html>");
    let c = explore_with(&h, None);
    assert_eq!(c.templates.len(), 1);
    let t = &c.templates[0];
    assert!(matches!(&t.effects[0], Effect::Attribute { target, .. } if target == "$t"), "{:?}", t.effects);
    assert!(t.instances.iter().all(|i| i.slots.is_empty()));
    assert_eq!(sorted(c.expanded_transitions()), sorted(unbounded(&h).expanded_transitions()));
}

/// ★ A sample that disagrees REFUSES the template: the samples' own
/// transitions are kept as ordinary ones, the examples stay per-element, and
/// the rest stay cut (represented), exactly as before templates existed.
#[test]
fn a_disagreeing_sample_refuses_the_template() {
    let mut h = String::from("<html><body>");
    for i in 0..100 { h.push_str(&format!("<button id=b{i} class=x>b</button>")); }
    h.push_str("<p id=o></p><script>var bs = document.getElementsByTagName('button');\n\
        for (var i = 0; i < bs.length; i++) bs[i].addEventListener('click', function () {\n\
          if (this.getAttribute('id') === 'b99') document.getElementById('o').setAttribute('class', 'last');\n\
          else this.className = 'x on'; });\n</script></body></html>");
    let c = explore_with(&h, None);
    assert!(c.templates.is_empty(), "{:?}", c.templates);
    let s = &c.explore_stats;
    assert_eq!((s.refused, s.verify_probes, s.predicted), (1, 3, 0), "{s:?}");
    // 64 examples and 3 samples, each its own transition; b99's is its own.
    assert_eq!(c.transitions.len(), 67);
    assert!(c.transitions.iter().any(|t| t.trigger == "#b99"
        && matches!(&t.effects[0], Effect::Attribute { target, .. } if target == "#o")));
    let cut = c.coverage.iter().filter(|k| k.over_cap && !k.probed).count();
    assert_eq!(cut, 33);
    assert!(c.coverage.iter().filter(|k| k.over_cap && !k.probed).all(|k| k.represented && !k.templated));
}

/// Content that differs per element (a definition panel listing that
/// definition's own references) is not one transition, and is not forced
/// into one: no template, every probe its own.
#[test]
fn per_element_content_is_not_templated() {
    let mut h = String::from("<html><body>");
    for i in 0..70 { h.push_str(&format!("<dfn data-n={i}>d</dfn>")); }
    h.push_str("<div id=o></div><script>var ds = document.getElementsByTagName('dfn');\n\
        for (var i = 0; i < ds.length; i++) ds[i].addEventListener('click', function () {\n\
          var p = document.createElement('p'); p.setText(this.getAttribute('data-n'));\n\
          document.getElementById('o').appendChild(p); });\n</script></body></html>");
    let c = explore_with(&h, None);
    assert!(c.templates.is_empty(), "{:?}", c.templates);
    assert_eq!(c.transitions.len(), 64);
    assert_eq!(c.explore_stats.refused, 0, "inference failed; nothing was predicted to refuse");
}

/// A sibling whose current state does not match what the effect starts
/// from is not predicted (it would record something else), and the rest
/// still are.
#[test]
fn a_sibling_already_in_the_target_state_is_not_predicted() {
    let mut h = String::from("<html><body>");
    for i in 0..80 { h.push_str(&format!("<button id=b{i} class=\"{}\">b</button>", if i == 70 { "x on" } else { "x" })); }
    h.push_str("<script>var bs = document.getElementsByTagName('button');\n\
        for (var i = 0; i < bs.length; i++) bs[i].addEventListener('click', function () { this.className = 'x on'; });\n\
        </script></body></html>");
    let c = explore_with(&h, None);
    assert_eq!(c.templates.len(), 1);
    let trig: Vec<&str> = c.templates[0].instances.iter().map(|i| i.trigger.as_str()).collect();
    assert!(!trig.contains(&"#b70"), "b70 is already on: its probe records nothing");
    assert_eq!(trig.len(), 79);
    assert_eq!(sorted(c.expanded_transitions()), sorted(unbounded(&h).expanded_transitions()));
}

/// The artifact: `/4` with a `templates` table only when there is one; a
/// recording without templates is `/3`, byte for byte what it was.
#[test]
fn the_artifact_is_v4_only_with_templates() {
    let html = tabs(100);
    let c = explore_with(&html, None);
    let a = artifact::emit(Some("https://example.test/p.html"), &c, &TierPolicy::default());
    assert!(a.contains("\"format\": \"atrium-navigator-recording/4\""), "{}", &a[..200]);
    assert!(a.contains("\"templates\": ["));
    assert!(a.contains("\"$0\""));
    let p = plain(&html);
    let b = artifact::emit(Some("https://example.test/p.html"), &p, &TierPolicy::default());
    assert!(b.contains("\"format\": \"atrium-navigator-recording/3\""));
    assert!(!b.contains("\"templates\""));
}

/// Recording is deterministic with templates.
#[test]
fn templated_recordings_are_reproducible() {
    let html = tabs(150);
    let (a, b) = (explore_with(&html, None), explore_with(&html, None));
    let p = TierPolicy::default();
    assert_eq!(artifact::emit(None, &a, &p), artifact::emit(None, &b, &p));
}

/// The verification budget is a bound: with too few probes left to verify,
/// nothing is predicted, and the examples still fold.
#[test]
fn predicted_instances_are_never_unverified() {
    let html = tabs(120);
    let c = explore_with(&html, None);
    let probed: usize = c.templates.iter().flat_map(|t| &t.instances).filter(|i| i.probed).count();
    assert_eq!(probed, 64 + 3);
    assert_eq!(c.explore_stats.verify_probes, 3, "every predicted instance is behind a verified sample");
}

/// ★ A handler that does nothing observable on any sibling it was probed on
/// is not a large state space. A sample of the cut siblings is probed too,
/// and if it is quiet as well the rest are `inert`: known, not cut.
#[test]
fn a_handler_quiet_on_every_sibling_is_verified_inert() {
    let mut h = String::from("<html><body>");
    for i in 0..90 { h.push_str(&format!("<a id=a{i} href=/x>a</a>")); }
    h.push_str("<script>var as = document.getElementsByTagName('a');\n\
        for (var i = 0; i < as.length; i++) as[i].addEventListener('click', function (e) { e.preventDefault(); });\n\
        </script></body></html>");
    let c = explore_with(&h, None);
    let s = &c.explore_stats;
    assert_eq!((s.verify_probes, s.inert, s.refused), (3, 90 - 64 - 3, 0), "{s:?}");
    assert!(c.templates.is_empty() && c.transitions.is_empty());
    let inert: Vec<_> = c.coverage.iter().filter(|k| k.inert).collect();
    assert_eq!(inert.len(), 23);
    assert!(inert.iter().all(|k| !k.probed && !k.represented && !k.templated));
}

/// And a sample that DOES something refuses the claim.
#[test]
fn a_sample_that_acts_refuses_inertness() {
    let mut h = String::from("<html><body>");
    for i in 0..90 { h.push_str(&format!("<a id=a{i} href=/x>a</a>")); }
    h.push_str("<p id=o></p><script>var as = document.getElementsByTagName('a');\n\
        for (var i = 0; i < as.length; i++) as[i].addEventListener('click', function (e) {\n\
          if (this.getAttribute('id') === 'a89') document.getElementById('o').setAttribute('class', 'x'); });\n\
        </script></body></html>");
    let c = explore_with(&h, None);
    let s = &c.explore_stats;
    assert_eq!((s.inert, s.refused), (0, 1), "{s:?}");
    assert!(c.transitions.iter().any(|t| t.trigger == "#a89"), "the sample's own transition is kept");
    assert!(c.coverage.iter().all(|k| !k.inert));
}

/// `el.id` is answered while exploring: a handler that keys on its own id
/// records the right thing, per element, and is NOT one template.
#[test]
fn a_handler_reading_its_own_id_sees_it() {
    let mut h = String::from("<html><body>");
    for i in 0..5 { h.push_str(&format!("<button id=b{i}>b</button><div id=b{i}-panel hidden>p</div>")); }
    h.push_str("<script>var bs = document.getElementsByTagName('button');\n\
        for (var i = 0; i < bs.length; i++) bs[i].addEventListener('click', function () {\n\
          document.getElementById(this.id + '-panel').removeAttribute('hidden'); });\n\
        </script></body></html>");
    let c = explore_with(&h, None);
    let all = c.expanded_transitions();
    assert_eq!(all.len(), 5, "{all:?}");
    for i in 0..5 {
        let t = all.iter().find(|t| t.trigger == format!("#b{i}")).unwrap();
        assert!(matches!(&t.effects[..], [Effect::Attribute { target, name, .. }] if *target == format!("#b{i}-panel") && name == "hidden"),
            "{t:?}");
    }
    // The load pass is unchanged: the artifact matches the non-exploring one.
    assert_eq!(c.html, plain(&h).html);
}

/// A probe whose effect is a NAVIGATION records nothing (navigation is
/// refused), and is now told apart from one that did nothing at all.
#[test]
fn a_refused_navigation_is_counted() {
    let h = "<html><body><button id=g>go</button><button id=q>q</button><script>\n\
        document.getElementById('g').addEventListener('click', function () { location.assign('/next'); });\n\
        document.getElementById('q').addEventListener('click', function () { });\n\
        </script></body></html>";
    let c = explore_with(h, None);
    let g = c.coverage.iter().find(|k| k.path == "#g").unwrap();
    let q = c.coverage.iter().find(|k| k.path == "#q").unwrap();
    assert_eq!((g.nav_refused, q.nav_refused), (1, 0), "{g:?}");
    assert!(!g.recorded);
    assert_eq!(c.html, plain(h).html);
}
