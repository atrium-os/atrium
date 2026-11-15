//! Slide transitions and builds (MS-PPT §2.5.13, §2.8.3, §2.8.4).
//!
//! A slide's transition is its `SlideShowSlideInfoAtom`. Builds come in two
//! shapes: PowerPoint 2002 and later keep a timing tree (as in a `.pptx`)
//! in the slide's `___PPT10` tag, and PowerPoint 97 keeps an
//! `AnimationInfoAtom` per shape. Both are written as a `<p:timing>` tree
//! naming each effect's preset, which the `.pptx` reader turns into builds.

use std::collections::HashMap;
use std::fmt::Write as _;

use officina_slides::model::{Dir, Transition, TransitionKind};

use super::rec::{i32_at, u16_at, u32_at, Rec};
use crate::report::{Kind, Report};

const TIME_NODE: u16 = 0xF144;
const TIME_NODE_ATOM: u16 = 0xF127;
const PROPERTIES: u16 = 0xF13D;
const VARIANT: u16 = 0xF142;
const CONDITION: u16 = 0xF125;
const CONDITION_ATOM: u16 = 0xF128;
const VISUAL_SHAPE: u16 = 0x2AFB;
pub const ANIMATION_INFO: u16 = 0x1014;
pub const ANIMATION_INFO_ATOM: u16 = 0x0FF1;

/// Most effects taken from one slide.
const MAX_EFFECTS: usize = 1024;

/// The transition a `SlideShowSlideInfoAtom` gives, if any.
pub fn transition(b: &[u8], rep: &mut Report) -> Option<Transition> {
    if b.len() < 13 {
        return None;
    }
    let (time, dir, kind, flags, speed) = (u32_at(b, 0), b[8], b[9], u16_at(b, 10), b[12]);
    let auto = flags & 0x400 != 0;
    let side = |d: u8| match d & 3 {
        0 => Dir::Left,
        1 => Dir::Up,
        2 => Dir::Right,
        _ => Dir::Down,
    };
    let kind = match kind {
        0 if !auto => return None,
        0 => TransitionKind::Cut,
        4 => TransitionKind::Cover(side(dir)),
        5 => TransitionKind::Dissolve,
        6 | 23 => TransitionKind::Fade,
        7 => TransitionKind::Uncover(side(dir)),
        10 => TransitionKind::Wipe(side(dir)),
        11 => TransitionKind::Zoom { out: dir != 0 },
        13 => TransitionKind::Split { vertical: dir >= 2, out: dir & 1 == 0 },
        // The file names the side the slide comes from; a push names the way it moves.
        20 => TransitionKind::Push(side(dir ^ 2)),
        _ => {
            rep.note(Kind::Approximate, "slide transitions not in the set shown as fades");
            TransitionKind::Fade
        }
    };
    let ms = match speed {
        0 => 1000,
        1 => 750,
        _ => 500,
    };
    Some(Transition { kind, ms, on_click: flags & 1 != 0 || !auto, after_ms: auto.then_some(time.min(3_600_000)) })
}

/// One effect, as the timing tree names it.
struct Fx {
    class: u32,
    preset: u32,
    sub: u32,
    node: u32,
    delay: u32,
    ms: u32,
    spid: u32,
}

fn variant(props: &Rec, inst: u16) -> Option<u32> {
    props.kids().find(|v| v.ty == VARIANT && v.inst == inst && v.body.first() == Some(&1)).map(|v| u32_at(v.body, 1))
}

fn effects(n: Rec, depth: usize, out: &mut Vec<Fx>) {
    if depth > 24 || out.len() >= MAX_EFFECTS {
        return;
    }
    let class = n.child(PROPERTIES).and_then(|p| variant(&p, 11).map(|c| (c, p)));
    if let Some((class, p)) = class {
        let delay = n.kids().find(|c| c.ty == CONDITION && c.inst == 1).and_then(|c| c.child(CONDITION_ATOM)).map(|a| i32_at(a.body, 12)).filter(|d| (0..=60_000).contains(d)).unwrap_or(0) as u32;
        let mut ms = 0;
        let mut spid = None;
        fn walk(r: Rec, depth: usize, ms: &mut u32, spid: &mut Option<u32>) {
            for k in r.kids() {
                match k.ty {
                    TIME_NODE_ATOM => {
                        let d = u32_at(k.body, 24);
                        if (1..=60_000).contains(&d) {
                            *ms = (*ms).max(d);
                        }
                    }
                    VISUAL_SHAPE if spid.is_none() => *spid = Some(u32_at(k.body, 8)),
                    _ if k.is_container() && depth < 12 => walk(k, depth + 1, ms, spid),
                    _ => {}
                }
            }
        }
        for k in n.kids().filter(|k| k.ty == TIME_NODE) {
            walk(k, 0, &mut ms, &mut spid);
        }
        if let Some(spid) = spid {
            out.push(Fx { class, preset: variant(&p, 9).unwrap_or(0), sub: variant(&p, 10).unwrap_or(0), node: variant(&p, 20).unwrap_or(1), delay, ms, spid });
        }
        return;
    }
    for k in n.kids().filter(|k| k.ty == TIME_NODE) {
        effects(k, depth + 1, out);
    }
}

/// The effects of a slide's `___PPT10` timing tree, in order.
fn tree_effects(slide: Rec) -> Vec<Fx> {
    let mut out = Vec::new();
    if let Some(tag) = super::tag_data(slide, "___PPT10") {
        for r in super::rec::recs(tag.body).filter(|r| r.ty == TIME_NODE) {
            effects(r, 0, &mut out);
        }
    }
    out
}

/// The effects of PowerPoint 97 builds: (order, `AnimationInfoAtom`, spid).
fn old_effects(anims: &mut [(u16, &[u8], u32)], rep: &mut Report) -> Vec<Fx> {
    anims.sort_by_key(|a| a.0);
    let mut out = Vec::new();
    for (_, b, spid) in anims.iter().take(MAX_EFFECTS) {
        let flags = u16_at(b, 4);
        let delay = i32_at(b, 12).clamp(0, 60_000) as u32;
        let (effect, dir) = (b.get(21).copied().unwrap_or(0), b.get(22).copied().unwrap_or(0));
        let sub = [8, 1, 2, 4][dir as usize & 3];
        let (preset, sub) = match effect {
            0 => (1, 0),
            5 | 6 => (10, 0),
            10 => (22, sub),
            11 => (53, 16),
            12 => (2, sub),
            _ => {
                rep.note(Kind::Approximate, "entrance effects not in the set run as fades");
                (10, 0)
            }
        };
        let node = if flags & 0x4 != 0 { 3 } else { 1 };
        out.push(Fx { class: 1, preset, sub, node, delay: if node == 3 { delay } else { 0 }, ms: 500, spid: *spid });
    }
    out
}

/// The slide's builds as a `<p:timing>` tree, shapes named by their ids
/// in the written slide; empty when there are none.
pub fn timing(slide: Rec, spids: &HashMap<u32, u32>, anims: &mut [(u16, &[u8], u32)], rep: &mut Report) -> String {
    let mut fx = tree_effects(slide);
    if fx.is_empty() && !anims.is_empty() {
        fx = old_effects(anims, rep);
    }
    let fx: Vec<(Fx, u32)> = fx.into_iter().filter_map(|f| Some((spids.get(&f.spid).copied()?, f))).map(|(id, f)| (f, id)).collect();
    if fx.is_empty() {
        return String::new();
    }
    let mut s = String::from(r#"<p:timing><p:tnLst><p:par><p:cTn id="1" dur="indefinite" restart="never" nodeType="tmRoot"><p:childTnLst><p:seq concurrent="1" nextAc="seek"><p:cTn id="2" dur="indefinite" nodeType="mainSeq"><p:childTnLst>"#);
    let mut n = 2;
    for (f, id) in fx {
        let class = ["entr", "entr", "exit", "emph", "path", "verb", "mediacall"].get(f.class as usize).copied().unwrap_or("emph");
        let node = match f.node {
            2 => "withEffect",
            3 => "afterEffect",
            _ => "clickEffect",
        };
        let _ = write!(
            s,
            r#"<p:par><p:cTn id="{}" presetID="{}" presetClass="{class}" presetSubtype="{}" fill="hold" nodeType="{node}"><p:stCondLst><p:cond delay="{}"/></p:stCondLst><p:childTnLst><p:set><p:cBhvr><p:cTn id="{}" dur="{}" fill="hold"/><p:tgtEl><p:spTgt spid="{id}"/></p:tgtEl><p:attrNameLst><p:attrName>style.visibility</p:attrName></p:attrNameLst></p:cBhvr><p:to><p:strVal val="visible"/></p:to></p:set></p:childTnLst></p:cTn></p:par>"#,
            n + 1,
            f.preset,
            f.sub,
            f.delay,
            n + 2,
            if f.ms == 0 { 500 } else { f.ms }
        );
        n += 2;
    }
    s.push_str("</p:childTnLst></p:cTn></p:seq></p:childTnLst></p:cTn></p:par></p:tnLst></p:timing>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        let mut rep = Report::default();
        // A fade at medium speed, by click.
        let fade = [0, 4, 0, 0, 0, 0, 0, 0, 0, 0x17, 0x11, 0, 1, 0, 0, 0];
        let t = transition(&fade, &mut rep).unwrap();
        assert_eq!((t.kind, t.ms, t.on_click, t.after_ms), (TransitionKind::Fade, 750, true, None));
        // A wipe downwards, slow, on by itself after 3 s; hidden too.
        let wipe = [0xB8, 0x0B, 0, 0, 0, 0, 0, 0, 3, 10, 0x14, 0x04, 0, 0, 0, 0];
        let t = transition(&wipe, &mut rep).unwrap();
        assert_eq!((t.kind, t.ms, t.on_click, t.after_ms), (TransitionKind::Wipe(Dir::Down), 1000, false, Some(3000)));
        // No effect and no timing: none. Cut short: none.
        assert!(transition(&[0; 16], &mut rep).is_none());
        assert!(transition(&fade[..8], &mut rep).is_none());
    }
}
