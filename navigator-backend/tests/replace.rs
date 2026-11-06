//! ★ REPLAY PART 2, HELD TO A FULL RELAYOUT (backend §4.3): after every
//! step and rewind of every recording in `NAVIGATOR_RECORDINGS`, the content
//! source updated in place (`DocumentSource::replace`: only from the
//! checkpoint above the change, only changed bands sent) must leave the view
//! holding exactly what a source built fresh on the new state gives it —
//! every resident band's range and NSG, the overlay, the extent — both at
//! once and after both have settled (every checkpoint and band range too).
//! The view's state is what the messages built (a mirror of puts and
//! clears), so a band that changed but was not sent is caught as well.
//!
//! A deliberately broken control (`bands::set_replace_control`: nothing is
//! laid out again) must be caught.

use navigator_backend::session::{SessionLimits, Sessions};
use navigator_backend::{ingest, Limits};
use navigator_render::fontset::FontSet;
use navigator_render::source::{ContentSource, DocumentSource, Event, GroupId, SceneSink, Want};
use navigator_style::cascade::Env;
use std::collections::BTreeMap;

#[derive(Default)]
struct Mirror<'a> { fonts: Option<&'a FontSet>, groups: BTreeMap<GroupId, (Option<(i64, i64)>, String)>, extent: Option<(i64, bool)>, puts: usize }

impl SceneSink for Mirror<'_> {
    fn put(&mut self, g: GroupId, clip: Option<(i64, i64)>, s: &navigator_render::Scene) {
        self.puts += 1;
        self.groups.insert(g, (clip, navigator_render::nsg::write(s, self.fonts.unwrap())));
    }
    fn clear(&mut self, g: GroupId) { self.groups.remove(&g); }
}

impl Mirror<'_> {
    fn events(&mut self, ev: &[Event]) {
        for e in ev { if let Event::Extent { height, settled } = e { self.extent = Some((*height, *settled)) } }
    }
}

/// Where two views differ, as a short description (`None`: identical).
fn differ(a: &Mirror, b: &Mirror) -> Option<String> {
    if a.extent != b.extent { return Some(format!("extent {:?} vs {:?}", a.extent, b.extent)) }
    // A provisional band's id is a running number (opaque to the view).
    let norm = |m: &Mirror| -> BTreeMap<GroupId, (Option<(i64, i64)>, String)> {
        m.groups.iter().map(|(g, v)| (match g { GroupId::Band(k) if *k >= navigator_render::source::PROVISIONAL => GroupId::Band(navigator_render::source::PROVISIONAL), g => *g }, v.clone())).collect()
    };
    let (ga, gb) = (norm(a), norm(b));
    let ka: Vec<_> = ga.keys().collect();
    let kb: Vec<_> = gb.keys().collect();
    if ka != kb { return Some(format!("groups {ka:?} vs {kb:?}")) }
    for (g, (c, s)) in &ga {
        let (c2, s2) = &gb[g];
        if c != c2 { return Some(format!("{g:?} clip {c:?} vs {c2:?}")) }
        if s != s2 {
            let at = s.bytes().zip(s2.bytes()).take_while(|(x, y)| x == y).count();
            return Some(format!("{g:?} NSG differs at byte {at} of {}/{}: {:?} vs {:?}", s.len(), s2.len(),
                &s[at.saturating_sub(60)..(at + 60).min(s.len())], &s2[at.saturating_sub(60)..(at + 60).min(s2.len())]));
        }
    }
    None
}

struct Totals { steps: usize, mismatches: Vec<String>, updated: usize, kept: usize }

/// Run every step and rewind of the recordings, with the view at `view_frac`
/// of the settled height (negative: where the reader sees each `#id`
/// trigger, before it is clicked). Returns what was checked.
fn run(dir: &str, view_frac: f64, broken: bool, max_docs: usize) -> Totals {
    navigator_render::bands::set_replace_control(broken);
    let fonts = FontSet::shared().expect("fonts");
    let env = Env::default();
    let vh = (env.height_px * 64.0) as i64;
    let mut t = Totals { steps: 0, mismatches: vec![], updated: 0, kept: 0 };
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("readable").flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    let mut docs = 0;
    for p in &files {
        let bytes = std::fs::read(p).expect("readable");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let triggers: Vec<String> = r.transitions.iter().filter(|t| t.anchored).map(|t| t.trigger.clone()).collect();
        if triggers.is_empty() { continue }
        if docs == max_docs { break }
        docs += 1;
        let json = std::str::from_utf8(&bytes).unwrap();
        let rec = navigator_normalize::recording::parse(json).expect("parses");
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let norm = |html: &str| navigator_normalize::normalize_and_measure(html, &rec.inputs, fonts, &env).0;
        let mut sessions = Sessions::new(SessionLimits::default());
        let Ok(id) = sessions.open(&r, 0) else { continue };
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let mut inc = DocumentSource::new(norm(&rec.document), fonts, subs.clone(), (env.width_px, env.height_px), vh);
        let mut view = Mirror { fonts: Some(fonts), ..Default::default() };
        // The view's place: a fraction of the settled height.
        let ev = inc.idle(1 << 20, &mut view);
        view.events(&ev);
        let y = ((inc.banded().extent().0 as f64 * view_frac.max(0.0)) as i64 / 64) * 64;
        let mut want = Want { range: (y, y + vh), zoom_64: 64, prefetch: vh };
        let ev = inc.want(want, &mut view);
        view.events(&ev);
        for trig in &triggers {
            if view_frac < 0.0 {
                if let Some(k) = trig.strip_prefix('#').and_then(|i| inc.banded().band_of_id(i)) {
                    let y = inc.banded().bands[k].range.0.max(0);
                    want = Want { range: (y, y + vh), zoom_64: 64, prefetch: vh };
                    let ev = inc.want(want, &mut view);
                    view.events(&ev);
                }
            }
            for forward in [true, false] {
                let s = sessions.get_mut(id, 0).expect("open");
                let ok = if forward { s.navigate(trig).is_ok() } else { s.back().is_ok() };
                if !ok { break }
                let html = norm(&s.document().dom.serialize());
                t.steps += 1;
                let puts0 = view.puts;
                let ev = inc.replace(html.clone(), None, &mut view);
                view.events(&ev);
                t.updated += ev.iter().filter(|e| matches!(e, Event::BandUpdated { .. })).count();
                if view.puts == puts0 { t.kept += 1 }
                // At once: a fresh source, its first pass as far as this one's.
                let mut fresh = DocumentSource::new(html.clone(), fonts, subs.clone(), (env.width_px, env.height_px), vh);
                let mut fv = Mirror { fonts: Some(fonts), ..Default::default() };
                // As far as this one kept (it lays out again from there).
                let kept = inc.replaced_from().unwrap_or(inc.banded().laid_out());
                if kept > 0 { let ev = fresh.idle(kept, &mut fv); fv.events(&ev); }
                let ev: Vec<Event> = vec![];
                fv.events(&ev);
                let ev = fresh.want(want, &mut fv);
                fv.events(&ev);
                if let Some(d) = differ(&view, &fv) { t.mismatches.push(format!("{name} {trig} {} at once: {d} | inc {} | fresh {}", if forward { "go" } else { "back" }, inc.banded().debug_state(), fresh.banded().debug_state())) }
                // Settled, both.
                let ev = inc.idle(1 << 20, &mut view);
                view.events(&ev);
                let ev = fresh.idle(1 << 20, &mut fv);
                fv.events(&ev);
                let (a, b) = (inc.banded(), fresh.banded());
                let ranges = |x: &navigator_render::bands::Banded| x.bands.iter().map(|b| b.range).collect::<Vec<_>>();
                if a.checkpoints != b.checkpoints || ranges(a) != ranges(b) {
                    t.mismatches.push(format!("{name} {trig} settled: checkpoints {} vs {}", a.checkpoints.len(), b.checkpoints.len()));
                } else if let Some(d) = differ(&view, &fv) {
                    t.mismatches.push(format!("{name} {trig} {} settled: {d}", if forward { "go" } else { "back" }));
                }
            }
        }
        eprintln!("{name}: {} steps so far, {} mismatches", t.steps, t.mismatches.len());
    }
    navigator_render::bands::set_replace_control(false);
    t
}

#[test]
fn incremental_relayout_is_a_full_relayout() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else { eprintln!("corpus: set NAVIGATOR_RECORDINGS"); return };
    let max_docs = std::env::var("REPLACE_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    for dir in dir.split(':') {
        let views: Vec<f64> = std::env::var("REPLACE_VIEWS").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or(vec![0.0, 0.33, -1.0]);
        for &frac in &views {
            let t = run(dir, frac, false, max_docs);
            eprintln!("{dir} view at {frac}: {} steps, {} with nothing sent, {} band_updated, {} mismatches", t.steps, t.kept, t.updated, t.mismatches.len());
            for m in t.mismatches.iter().take(20) { eprintln!("  {m}") }
            assert!(t.mismatches.is_empty(), "{} of {} steps differ from a full relayout", t.mismatches.len(), t.steps);
        }
    }
}

#[test]
fn the_broken_control_is_caught() {
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else { eprintln!("corpus: set NAVIGATOR_RECORDINGS"); return };
    let dir = dir.split(':').next().unwrap();
    let t = run(dir, 0.0, true, 6);
    eprintln!("control: {} steps, {} mismatches", t.steps, t.mismatches.len());
    assert!(!t.mismatches.is_empty(), "laying nothing out again went unnoticed over {} steps", t.steps);
}

/// ★ THE WORKER'S PATH (`display::Bands::update`): the proven-inert steps,
/// the cached states (a step back; a click laid out ahead) and the misses,
/// each held to a source built fresh on the new state, settled, through the
/// answers the worker sends (puts, clears, events) applied to a view.
#[test]
fn the_workers_update_is_a_full_relayout() {
    use navigator_backend::display::{Bands, BandsAnswer, BandsAsk, UpdateKind};
    let Ok(dirs) = std::env::var("NAVIGATOR_RECORDINGS") else { eprintln!("corpus: set NAVIGATOR_RECORDINGS"); return };
    let max_docs = std::env::var("REPLACE_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let fonts = FontSet::shared().expect("fonts");
    let vh = (Env::default().height_px * 64.0) as i64;
    let want = BandsAsk::Want { range: (0, vh), zoom_64: 64, prefetch: vh };
    let apply = |m: &mut BTreeMap<GroupId, (Option<(i64, i64)>, String)>, ext: &mut Option<(i64, bool)>, a: BandsAnswer| {
        for c in a.clears { m.remove(&c); }
        for p in a.puts { m.insert(p.group, (p.clip, p.nsg)); }
        for e in a.events { if let Event::Extent { height, settled } = e { *ext = Some((height, settled)) } }
    };
    let settle = |b: &mut Bands, m: &mut BTreeMap<GroupId, (Option<(i64, i64)>, String)>, ext: &mut Option<(i64, bool)>| {
        for _ in 0..100_000 { let a = b.ask(BandsAsk::Idle { bands: 64 }).unwrap(); apply(m, ext, a); if b.settled() { break } }
        let a = b.ask(want).unwrap();
        apply(m, ext, a);
    };
    let (mut steps, mut bad, mut kinds) = (0usize, vec![], BTreeMap::<String, usize>::new());
    for dir in dirs.split(':') {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("readable").flatten().map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        let mut docs = 0;
        for p in &files {
            let bytes = std::fs::read(p).expect("readable");
            let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
            let triggers: Vec<String> = r.transitions.iter().filter(|t| t.anchored).map(|t| t.trigger.clone()).collect();
            if triggers.is_empty() { continue }
            if docs == max_docs { break }
            docs += 1;
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let mut sessions = Sessions::new(SessionLimits::default());
            let Ok(id) = sessions.open(&r, 0) else { continue };
            let (mut b, _) = Bands::open(&bytes, None, None, 0).expect("opens");
            { let s = sessions.get(id).unwrap(); b.showing(s.document(), s.state_key()); }
            let (mut view, mut ext) = (BTreeMap::new(), None);
            let a = b.ask(want).unwrap();
            apply(&mut view, &mut ext, a);
            let mut gen = 0;
            for (i, trig) in triggers.iter().enumerate() {
                for forward in [true, false] {
                    // Every other click foreseen (laid out ahead, as idle time would).
                    if forward && i % 2 == 0 && std::env::var_os("NO_PRECOMPUTE").is_none() {
                        let s = sessions.get(id).unwrap();
                        let next: Vec<_> = s.next_states().into_iter().filter(|(t, _)| t.trigger == *trig).collect();
                        b.precompute(&bytes, s.document(), &next, 1).unwrap();
                    }
                    let s = sessions.get_mut(id, 0).unwrap();
                    let ok = if forward { s.navigate(trig).is_ok() } else { s.back().is_ok() };
                    if !ok { break }
                    gen += 1;
                    let s = sessions.get(id).unwrap();
                    let k = b.update(&bytes, s.document(), s.state_key(), gen).unwrap();
                    assert_ne!(k, UpdateKind::Rebuild);
                    *kinds.entry(format!("{k:?}")).or_default() += 1;
                    steps += 1;
                    let a = b.ask(want).unwrap();
                    if std::env::var_os("STEP_LOG").is_some() { eprintln!("STEP {i} {} {k:?} key {:x} puts {} events {:?}", if forward { "go" } else { "back" }, s.state_key(), a.puts.len(), a.events) }
                    apply(&mut view, &mut ext, a);
                    settle(&mut b, &mut view, &mut ext);
                    let (mut f, _) = Bands::open(&bytes, Some(&s.document().dom.serialize()), None, gen).unwrap();
                    let (mut fv, mut fext) = (BTreeMap::new(), None);
                    let a = f.ask(want).unwrap();
                    apply(&mut fv, &mut fext, a);
                    settle(&mut f, &mut fv, &mut fext);
                    let ma = Mirror { fonts: Some(fonts), groups: view.clone(), extent: ext, puts: 0 };
                    let mb = Mirror { fonts: Some(fonts), groups: fv, extent: fext, puts: 0 };
                    if let Some(d) = differ(&ma, &mb) {
                        if let Ok(dir) = std::env::var("DUMP_DIR") {
                            for (g, (_, nsg)) in &ma.groups { let _ = std::fs::write(format!("{dir}/{}-inc-{g:?}.nsg", bad.len()), nsg); }
                            for (g, (_, nsg)) in &mb.groups { let _ = std::fs::write(format!("{dir}/{}-fresh-{g:?}.nsg", bad.len()), nsg); }
                        }
                        bad.push(format!("{name} {trig} {} ({k:?}): {d}", if forward { "go" } else { "back" }))
                    }
                }
            }
            eprintln!("{name}: {steps} steps so far, {} mismatches, {kinds:?}", bad.len());
        }
    }
    eprintln!("worker path: {steps} steps {kinds:?}, {} mismatches", bad.len());
    for m in bad.iter().take(20) { eprintln!("  {m}") }
    assert!(bad.is_empty(), "{} of {steps} worker updates differ from a full relayout", bad.len());
}
