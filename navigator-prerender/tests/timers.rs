use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

/// The point: content built in a deferred callback must reach the artifact.
#[test]
fn deferred_content_is_produced() {
    let html = "<html><body><div id=r></div><script>\
        setTimeout(function(){\
          var p = document.createElement('p'); p.textContent='deferred';\
          document.getElementById('r').appendChild(p);\
        }, 0);\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("deferred"), "timer never ran: {}", c.html);
    assert!(c.timers_fired >= 1);
}

/// Ordering is by (virtual time, insertion), so it is total and reproducible.
#[test]
fn timers_run_in_virtual_time_order_and_deterministically() {
    let html = "<html><body><div id=r></div><script>\
        var out='';\
        setTimeout(function(){ out+='c'; document.getElementById('r').setAttribute('o',out); }, 20);\
        setTimeout(function(){ out+='a'; }, 0);\
        setTimeout(function(){ out+='b'; }, 10);\
        </script></body></html>";
    let a = run(html);
    assert!(a.html.contains(r#"o="abc""#), "wrong order: {}", a.html);
    assert_eq!(a.html, run(html).html, "timer dispatch must be deterministic");
}

/// A self-rescheduling timer is an ordinary idiom and must not hang the
/// converter: the callback budget stops it.
#[test]
fn runaway_rescheduler_terminates() {
    let html = "<html><body><div id=r></div><script>\
        var n=0;\
        function again(){ n++; document.getElementById('r').setAttribute('n',String(n)); setTimeout(again,1); }\
        again();\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.timers_fired <= navigator_prerender::boa_impl::TIMER_BUDGET,
        "budget exceeded: {}", c.timers_fired);
    assert!(c.timers_fired > 10, "should have run many: {}", c.timers_fired);
}

/// Beyond the horizon a timer legitimately never runs — a converter snapshots
/// shortly after load, and a banner scheduled for +60s would not be in a
/// screenshot either.
#[test]
fn timers_past_the_horizon_do_not_run() {
    let html = "<html><body><div id=r>base</div><script>\
        setTimeout(function(){ document.getElementById('r').setAttribute('late','yes'); }, 60000);\
        setTimeout(function(){ document.getElementById('r').setAttribute('soon','yes'); }, 100);\
        </script></body></html>";
    let c = run(html);
    assert!(c.html.contains(r#"soon="yes""#), "in-horizon timer must run: {}", c.html);
    assert!(!c.html.contains(r#"late="yes""#), "beyond-horizon timer must not: {}", c.html);
    assert!(c.timers_dropped >= 1, "the dropped one must be reported");
}

#[test]
fn clear_timeout_and_intervals_work() {
    let html = "<html><body><div id=r></div><script>\
        var id = setTimeout(function(){ document.getElementById('r').setAttribute('bad','1'); }, 5);\
        clearTimeout(id);\
        var k=0, iv = setInterval(function(){ k++; if(k>=3) clearInterval(iv);\
            document.getElementById('r').setAttribute('k',String(k)); }, 2);\
        </script></body></html>";
    let c = run(html);
    assert!(!c.html.contains(r#"bad="1""#), "cleared timer ran: {}", c.html);
    assert!(c.html.contains(r#"k="3""#), "interval should repeat then stop: {}", c.html);
}

/// requestAnimationFrame is a deferred callback too, and pages use it for
/// exactly the same purpose.
#[test]
fn raf_and_microtasks_run() {
    let html = "<html><body><div id=r></div><script>\
        requestAnimationFrame(function(){ document.getElementById('r').setAttribute('raf','1'); });\
        queueMicrotask(function(){ document.getElementById('r').setAttribute('micro','1'); });\
        </script></body></html>";
    let c = run(html);
    assert!(c.html.contains(r#"raf="1""#), "rAF did not run: {}", c.html);
    assert!(c.html.contains(r#"micro="1""#), "microtask did not run: {}", c.html);
}
