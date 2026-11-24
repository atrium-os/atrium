//! Artifex's full views (dev-tools.md §3.5): Fuzz and Composition take the
//! area right of the view strip, in place of the tree and the editor.
//! They draw [`State`] only; clicks become [`Act`]s like the rest of the
//! window.

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, radius, type_size, Semantic as Theme, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor, Button};
use pergola::Color;

use crate::composition::Level;
use crate::ui::{em, mono, plain, style, text, Act, Queue, State};

/// Which view fills the window right of the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Main {
    /// The tree, the tabs and the editor.
    #[default]
    Code,
    Composition,
    Fuzz,
    Assets,
}

/// The preview pane's width (Move into a worker).
pub fn preview_w() -> f32 {
    em(360.0)
}

/// The fuzz view's list of targets and crashes.
pub fn fuzz_list_w() -> f32 {
    em(300.0)
}

fn icon(ctx: &mut Ctx, x: f32, cy: f32, ch: char, size: f32, color: Color) {
    let h = em(size) * 1.2;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: ch.to_string(), style: style(font::ICONS, size, Weight::Regular, color) });
}

/// `s` cut to `width` as it measures in `st`.
fn cut(s: &str, width: f32, st: &pergola::TextStyle) -> String {
    if Node::measure_text(s, st).w <= width {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let t: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
        if Node::measure_text(&t, st).w <= width {
            return t;
        }
    }
    "…".into()
}

/// `s` cut to `width` of text at `size` (sans, measured).
fn fit(s: &str, width: f32, size: f32) -> String {
    cut(s, width, &style(font::SANS, size, Weight::Semibold, Color::TRANSPARENT))
}

/// The same for monospaced text.
fn fitm(s: &str, width: f32) -> String {
    cut(s, width, &style(font::MONO, type_size::SM, Weight::Regular, Color::TRANSPARENT))
}

/// `s` in lines of at most `width`, at most `max` of them (the last cut).
pub fn wrap(s: &str, width: f32, st: &pergola::TextStyle, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let words: Vec<&str> = s.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        let next = if cur.is_empty() { w.to_string() } else { format!("{cur} {w}") };
        if cur.is_empty() || Node::measure_text(&next, st).w <= width {
            cur = next;
            continue;
        }
        lines.push(std::mem::take(&mut cur));
        if lines.len() == max {
            let rest = words[i..].join(" ");
            let last = lines.pop().unwrap_or_default();
            lines.push(cut(&format!("{last} {rest}"), width, st));
            return lines;
        }
        cur = w.to_string();
    }
    if !cur.is_empty() {
        lines.push(cut(&cur, width, st));
    }
    lines
}

/// A paragraph of up to `max` lines from `cy` (the first line's middle);
/// returns how far down it went.
#[allow(clippy::too_many_arguments)]
fn para(ctx: &mut Ctx, x: f32, cy: f32, s: &str, width: f32, size: f32, weight: Weight, color: Color, max: usize) -> f32 {
    let st = style(font::SANS, size, weight, color);
    let lh = em(size) * 1.45;
    let lines = wrap(s, width, &st, max);
    for (i, l) in lines.iter().enumerate() {
        text(ctx, x, cy + i as f32 * lh, l, size, weight, color);
    }
    lines.len().max(1) as f32 * lh
}

/// A small capitals heading over a section.
fn heading(ctx: &mut Ctx, x: f32, cy: f32, s: &str, t: Theme) {
    text(ctx, x, cy, s, type_size::XXS, Weight::Semibold, t.text_tertiary());
}

/// A box with a 1-point border.
fn framed(ctx: &mut Ctx, r: Rect, fill: Color, border: Color, rad: f32) {
    ctx.add(Node::Rect { rect: r, fill: border, radius: rad });
    ctx.add(Node::Rect { rect: Rect::new(r.x() + 1.0, r.y() + 1.0, r.w() - 2.0, r.h() - 2.0), fill, radius: (rad - 1.0).max(0.0) });
}

/// A row that is a button: hover fill, click.
fn row(ctx: &mut Ctx, r: Rect, selected: bool, t: Theme, q: &Queue, a: Act) {
    let id = ctx.push(Node::Rect { rect: r, fill: Color::TRANSPARENT, radius: radius::SM });
    let fill = if selected { t.selection_idle() } else if ctx.is_hovered(id) { t.state_hover() } else { Color::TRANSPARENT };
    if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
        *f = fill;
    }
    ctx.on_click(id, plain(q, a));
}

/// `line` of a `file:line` frame.
fn frame_line(frame: &str) -> Option<usize> {
    frame.rsplit_once(':').and_then(|(_, l)| l.parse().ok())
}

/// A hex dump: offset, 16 bytes, their printable characters.
pub fn hex_lines(bytes: &[u8], per: usize) -> Vec<String> {
    let per = per.max(1);
    bytes.chunks(per).enumerate().map(|(i, c)| {
        let hex: Vec<String> = c.iter().map(|b| format!("{b:02x}")).collect();
        let chars: String = c.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        format!("{:08x}  {:<w$}  {chars}", i * per, hex.join(" "), w = per * 3 - 1)
    }).collect()
}

/// The fuzz view: targets and crashes on the left, the chosen crash in the
/// middle, and Move into a worker's preview on the right while it waits.
pub fn fuzz(ctx: &mut Ctx, s: &State, q: &Queue, area: Rect) {
    let t = ctx.theme;
    let (x0, y0, x1, y1) = (area.x(), area.y(), area.x() + area.w(), area.y() + area.h());
    let lw = fuzz_list_w();
    ctx.add(Node::Rect { rect: Rect::new(x0, y0, lw, area.h()), fill: t.bg_surface(), radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(x0 + lw - 1.0, y0, 1.0, area.h()), fill: t.border_default(), radius: 0.0 });
    text(ctx, x0 + em(16.0), y0 + em(26.0), "Fuzzing", type_size::LG, Weight::Semibold, t.text_primary());
    let Some(f) = &s.fuzz else {
        text(ctx, x0 + em(16.0), y0 + em(60.0), "This project has no atrium.toml: nothing to fuzz.", type_size::SM, Weight::Regular, t.text_secondary());
        return;
    };
    if f.listed && !f.targets.is_empty() {
        ctx.key(8100);
        let b = if f.on { Button::secondary("Stop fuzzing").lead(phosphor::PAUSE) } else { Button::primary("Start fuzzing").lead(phosphor::PLAY) };
        b.at(x0 + lw - em(146.0), y0 + em(10.0)).width(em(134.0)).on_click(plain(q, Act::Fuzz)).render(ctx);
    }
    let mut y = y0 + em(56.0);
    if let Some(e) = &f.error {
        icon(ctx, x0 + em(16.0), y + em(10.0), phosphor::WARNING, type_size::SM, t.warning_fg());
        text(ctx, x0 + em(36.0), y + em(10.0), &fit(e, lw - em(48.0), type_size::SM), type_size::SM, Weight::Regular, t.text_secondary());
        y += em(28.0);
    } else if !f.listed {
        text(ctx, x0 + em(16.0), y + em(10.0), "Asking for fuzz targets…", type_size::SM, Weight::Regular, t.text_secondary());
        y += em(28.0);
    }
    heading(ctx, x0 + em(16.0), y + em(6.0), "TARGETS", t);
    y += em(18.0);
    for (i, tg) in f.targets.iter().enumerate() {
        let r = Rect::new(x0 + em(8.0), y, lw - em(16.0), em(50.0));
        let crashes = f.crashes.iter().filter(|c| c.target == tg.name).count();
        let running = f.running.as_deref() == Some(tg.name.as_str());
        framed(ctx, r, t.bg_canvas(), if crashes > 0 && tg.core { t.danger_fg() } else { t.border_default() }, radius::MD);
        ctx.key(8200 + i as u64);
        let (ch, c) = if tg.core { (phosphor::SHIELD_CHECK, t.accent_mark()) } else { (phosphor::PUZZLE_PIECE, t.text_secondary()) };
        icon(ctx, r.x() + em(10.0), r.y() + em(17.0), ch, type_size::MD, c);
        text(ctx, r.x() + em(34.0), r.y() + em(17.0), &fit(&tg.name, r.w() - em(130.0), type_size::SM), type_size::SM, Weight::Medium, t.text_primary());
        let (tag, tag_c) = if crashes > 0 {
            (if crashes == 1 { "1 crash".to_string() } else { format!("{crashes} crashes") }, t.danger_fg())
        } else if running {
            ("running".to_string(), t.success_fg())
        } else {
            (String::new(), t.text_tertiary())
        };
        if !tag.is_empty() {
            let tw = em(7.0) * tag.len() as f32 + em(14.0);
            ctx.add(Node::Rect { rect: Rect::new(r.x() + r.w() - tw - em(8.0), r.y() + em(8.0), tw, em(18.0)), fill: tag_c.with_alpha(0.14), radius: em(9.0) });
            text(ctx, r.x() + r.w() - tw - em(1.0), r.y() + em(17.0), &tag, type_size::XXS, Weight::Semibold, tag_c);
        }
        let about = if running {
            format!("{} runs", f.execs)
        } else if tg.about.is_empty() {
            if tg.core { "the core's input".into() } else { "a worker: holds nothing".into() }
        } else {
            tg.about.clone()
        };
        text(ctx, r.x() + em(34.0), r.y() + em(36.0), &fit(&about, r.w() - em(44.0), type_size::XS), type_size::XS, Weight::Regular, t.text_secondary());
        y += em(56.0);
        if y > y1 - em(120.0) {
            break;
        }
    }
    if let Some((tg, m)) = &f.run_error {
        icon(ctx, x0 + em(16.0), y + em(10.0), phosphor::WARNING, type_size::SM, t.warning_fg());
        text(ctx, x0 + em(36.0), y + em(10.0), &fit(&format!("{tg} didn't run: {m}"), lw - em(48.0), type_size::XS), type_size::XS, Weight::Regular, t.text_secondary());
        y += em(26.0);
    }
    if !f.crashes.is_empty() {
        y += em(6.0);
        heading(ctx, x0 + em(16.0), y + em(6.0), "CRASHES", t);
        y += em(16.0);
        for (i, c) in f.crashes.iter().enumerate() {
            if y > y1 - em(70.0) {
                break;
            }
            ctx.key(8300 + i as u64);
            let r = Rect::new(x0 + em(8.0), y, lw - em(16.0), em(26.0));
            row(ctx, r, s.crash == i, t, q, Act::SelectCrash(i));
            icon(ctx, r.x() + em(8.0), r.y() + em(13.0), phosphor::WARNING_OCTAGON, type_size::SM, if c.in_core { t.danger_fg() } else { t.warning_fg() });
            let at = c.frame.as_deref().unwrap_or(&c.target);
            text(ctx, r.x() + em(30.0), r.y() + em(13.0), &fit(at, r.w() - em(40.0), type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
            ctx.pop();
            y += em(28.0);
        }
    }
    // What a round is, at the foot of the list.
    let mins = crate::fuzz::ROUND_SECS / 60;
    icon(ctx, x0 + em(16.0), y1 - em(40.0), phosphor::LIGHTNING, type_size::SM, t.text_tertiary());
    text(ctx, x0 + em(36.0), y1 - em(40.0), "Fuzzing runs in the workspace, in the", type_size::XS, Weight::Regular, t.text_tertiary());
    text(ctx, x0 + em(36.0), y1 - em(24.0), &format!("background: {mins} minutes a target a round."), type_size::XS, Weight::Regular, t.text_tertiary());

    // The chosen crash.
    let preview = s.moving.as_ref().filter(|m| m.dry_run);
    let mx = x0 + lw;
    let mw = x1 - mx - if preview.is_some() { preview_w() } else { 0.0 };
    let pad = em(24.0);
    let cw = mw - 2.0 * pad;
    let cx = mx + pad;
    match f.crashes.get(s.crash).or(f.crashes.first()) {
        None => {
            let (head, sub) = if !f.listed || f.error.is_some() {
                ("Nothing fuzzed yet", String::new())
            } else if f.on {
                ("No crashes yet", match &f.running { Some(tg) => format!("Fuzzing {tg}: {} runs so far.", f.execs), None => "Between rounds.".into() })
            } else {
                ("No crashes found", "Start fuzzing to try the core's inputs and every worker with generated bytes.".into())
            };
            icon(ctx, cx, y0 + em(40.0), phosphor::CHECK_CIRCLE, type_size::LG, t.success_fg());
            text(ctx, cx + em(30.0), y0 + em(40.0), head, type_size::LG, Weight::Semibold, t.text_primary());
            text(ctx, cx, y0 + em(68.0), &fit(&sub, cw, type_size::SM), type_size::SM, Weight::Regular, t.text_secondary());
        }
        Some(c) => {
            let i = f.crashes.iter().position(|x| std::ptr::eq(x, c)).unwrap_or(0);
            let about = f.targets.iter().find(|tg| tg.name == c.target).map(|tg| tg.about.clone()).filter(|a| !a.is_empty());
            let title = if c.in_core {
                format!("The core crashed on {}", about.as_deref().unwrap_or(&c.target))
            } else {
                format!("A worker crashed: {}", c.target)
            };
            icon(ctx, cx, y0 + em(36.0), phosphor::WARNING_OCTAGON, type_size::LG, if c.in_core { t.danger_fg() } else { t.warning_fg() });
            // Fixed room for two lines of title and two of summary, so the
            // buttons stay where they are.
            para(ctx, cx + em(30.0), y0 + em(36.0), &title, cw - em(30.0), type_size::LG, Weight::Semibold, t.text_primary(), 2);
            let at = c.frame.as_deref().map(|fr| format!(" at {fr}")).unwrap_or_default();
            para(ctx, cx + em(30.0), y0 + em(90.0), &format!("{}{at}", c.summary), cw - em(30.0), type_size::SM, Weight::Regular, t.text_secondary(), 2);
            let (mut bx, mut by) = (cx, y0 + em(132.0));
            let mut place = |w: f32| {
                if bx > cx && bx + w > cx + cw {
                    bx = cx;
                    by += em(40.0);
                }
                let at = (bx, by);
                bx += w + em(8.0);
                at
            };
            if c.in_core && c.frame.is_some() {
                let (x, y) = place(em(172.0));
                ctx.key(8400);
                Button::primary("Move into a worker").lead(phosphor::PUZZLE_PIECE).at(x, y).width(em(172.0))
                    .on_click(plain(q, Act::MoveToWorker(i))).render(ctx);
            }
            if c.binary.is_some() {
                let (x, y) = place(em(184.0));
                ctx.key(8401);
                Button::secondary("Debug with this input").lead(phosphor::BUG).at(x, y).width(em(184.0))
                    .on_click(plain(q, Act::ReplayCrash(i))).render(ctx);
            }
            if let Some(l) = c.frame.as_deref().and_then(frame_line) {
                let (x, y) = place(em(130.0));
                ctx.key(8402);
                Button::ghost(format!("Go to line {l}")).lead(phosphor::CODE).at(x, y).width(em(130.0))
                    .on_click(plain(q, Act::Crash(i))).render(ctx);
            }
            let mut y = by + em(52.0);
            let fix = c.fix.clone().or_else(|| c.in_core.then(|| "Move into a worker: parse it there, and send the core a message".to_string()));
            if let Some(fix) = fix {
                let why = if c.in_core { "The core holds the app's grants. " } else { "" };
                let msg = format!("{why}{fix}");
                let lines = wrap(&msg, cw - em(44.0), &style(font::SANS, type_size::SM, Weight::Regular, t.text_primary()), 3).len().max(1);
                let r = Rect::new(cx, y, cw, em(16.0) + lines as f32 * em(19.0));
                ctx.add(Node::Rect { rect: r, fill: if c.in_core { t.danger_fg().with_alpha(0.08) } else { t.accent_bg() }, radius: radius::SM });
                icon(ctx, cx + em(10.0), y + em(18.0), phosphor::LIGHTNING, type_size::SM, if c.in_core { t.danger_fg() } else { t.accent_fg() });
                para(ctx, cx + em(32.0), y + em(18.0), &msg, cw - em(44.0), type_size::SM, Weight::Regular, t.text_primary(), 3);
                y += r.h() + em(18.0);
            }
            heading(ctx, cx, y, &fit(&format!("THE INPUT · {}", c.input), cw, type_size::XXS), t);
            y += em(12.0);
            let lines = match &s.crash_bytes {
                // 16 bytes a line where it fits (75 characters of mono), else 8.
                Some((input, bytes)) if *input == c.input => hex_lines(bytes, if cw > em(640.0) { 16 } else { 8 }),
                _ => vec!["(reading the input…)".to_string()],
            };
            let n = lines.len().min(if cw > em(640.0) { 8 } else { 10 });
            let r = Rect::new(cx, y, cw, em(14.0) + n as f32 * em(18.0));
            framed(ctx, r, t.bg_surface(), t.border_default(), radius::SM);
            for (k, l) in lines.iter().take(n).enumerate() {
                mono(ctx, cx + em(12.0), y + em(16.0) + k as f32 * em(18.0), &fitm(l, cw - em(24.0)), t.text_primary());
            }
            y += r.h() + em(24.0);
            if y < y1 - em(60.0) {
                heading(ctx, cx, y, "WHERE", t);
                y += em(20.0);
                if let Some(fr) = &c.frame {
                    mono(ctx, cx + em(4.0), y, "0", t.text_tertiary());
                    mono(ctx, cx + em(24.0), y, &fitm(fr, cw - em(30.0)), t.text_primary());
                    y += em(20.0);
                }
                mono(ctx, cx + em(4.0), y, if c.frame.is_some() { "1" } else { "0" }, t.text_tertiary());
                mono(ctx, cx + em(24.0), y, &fitm(&format!("fuzz target {}", c.target), cw - em(30.0)), t.text_secondary());
            }
        }
    }

    // Move into a worker: what it would do, and the person's answer.
    if let Some(m) = preview {
        let px = x1 - preview_w();
        let pw = preview_w();
        ctx.add(Node::Rect { rect: Rect::new(px, y0, pw, area.h()), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(px, y0, 1.0, area.h()), fill: t.border_default(), radius: 0.0 });
        let ix = px + em(20.0);
        let iw = pw - em(40.0);
        let from = m.from.strip_prefix("crate::").unwrap_or(&m.from);
        text(ctx, ix, y0 + em(28.0), &fit(&format!("Move {from} into a worker"), iw, type_size::MD), type_size::MD, Weight::Semibold, t.text_primary());
        para(ctx, ix, y0 + em(50.0), &format!("A preview of cargo atrium worker add {}. Nothing is written until you press Move.", m.worker),
            iw, type_size::XS, Weight::Regular, t.text_secondary(), 2);
        heading(ctx, ix, y0 + em(92.0), "WORKER", t);
        framed(ctx, Rect::new(ix, y0 + em(102.0), iw, em(30.0)), t.bg_canvas(), t.border_default(), radius::SM);
        icon(ctx, ix + em(8.0), y0 + em(117.0), phosphor::PUZZLE_PIECE, type_size::SM, t.text_secondary());
        mono(ctx, ix + em(30.0), y0 + em(117.0), &fitm(&m.worker, iw - em(40.0)), t.text_primary());
        let mut y = y0 + em(156.0);
        let foot = y1 - em(60.0);
        heading(ctx, ix, y, "CHANGES", t);
        y += em(20.0);
        for c in &m.changed {
            if y > foot - em(40.0) {
                break;
            }
            let (action, path) = c.split_once(' ').unwrap_or(("", c.as_str()));
            let new = matches!(action, "create" | "created" | "new" | "add" | "added");
            icon(ctx, ix, y, if new { phosphor::PLUS } else { phosphor::PENCIL_SIMPLE }, type_size::SM, if new { t.success_fg() } else { t.warning_fg() });
            mono(ctx, ix + em(22.0), y, &fitm(path, iw - em(90.0)), t.text_primary());
            text(ctx, px + pw - em(20.0) - em(6.5) * action.len() as f32, y, action, type_size::XS, Weight::Regular, t.text_tertiary());
            y += em(22.0);
        }
        if !m.calls.is_empty() && y < foot - em(60.0) {
            y += em(8.0);
            heading(ctx, ix, y, &format!("CALLS TO CHANGE BY HAND ({})", m.calls.len()), t);
            y += em(18.0);
            if !m.core_call.is_empty() {
                text(ctx, ix, y, &fit(&format!("to {}", m.core_call), iw, type_size::XS), type_size::XS, Weight::Regular, t.text_secondary());
                y += em(18.0);
            }
            for c in &m.calls {
                if y > foot - em(20.0) {
                    break;
                }
                mono(ctx, ix, y, &fitm(c, iw), t.text_primary());
                y += em(20.0);
            }
        }
        for n in &m.notes {
            if y > foot - em(20.0) {
                break;
            }
            y += em(4.0);
            text(ctx, ix, y, &fit(n, iw, type_size::XS), type_size::XS, Weight::Regular, t.text_secondary());
            y += em(16.0);
        }
        ctx.add(Node::Rect { rect: Rect::new(px, foot, pw, 1.0), fill: t.border_default(), radius: 0.0 });
        ctx.key(8500);
        Button::ghost("Cancel").at(x1 - em(204.0), y1 - em(46.0)).width(em(84.0)).on_click(plain(q, Act::ConfirmMove(false))).render(ctx);
        ctx.key(8501);
        Button::primary("Move").lead(phosphor::PUZZLE_PIECE).at(x1 - em(112.0), y1 - em(46.0)).width(em(96.0))
            .on_click(plain(q, Act::ConfirmMove(true))).render(ctx);
    }
}

/// The icon a consent or capability line gets.
fn grant_icon(line: &str) -> char {
    let l = line.to_ascii_lowercase();
    if l.contains("network") || l.contains("internet") || l.contains("web") {
        phosphor::GLOBE
    } else if l.contains("secret") || l.contains("arca") || l.contains("key") || l.contains("password") {
        phosphor::KEY
    } else if l.contains("folder") || l.contains("file") || l.contains("picture") || l.contains("document") {
        phosphor::FOLDER
    } else {
        phosphor::SHIELD_CHECK
    }
}

/// The composition view: what the person grants, the core that holds it,
/// the workers that hold nothing, and the check's findings.
pub fn composition(ctx: &mut Ctx, s: &State, q: &Queue, area: Rect) {
    let t = ctx.theme;
    let (x0, y0, x1, y1) = (area.x(), area.y(), area.x() + area.w(), area.y() + area.h());
    let Some(c) = &s.composition else {
        text(ctx, x0 + em(24.0), y0 + em(40.0), "This project has no atrium.toml: it isn't an app.", type_size::MD, Weight::Regular, t.text_secondary());
        return;
    };
    // The header.
    let name = if !c.name.is_empty() { c.name.clone() } else if !c.project.is_empty() { c.project.clone() } else { c.id.clone() };
    icon(ctx, x0 + em(20.0), y0 + em(26.0), phosphor::GRAPH, type_size::LG, t.accent_mark());
    text(ctx, x0 + em(50.0), y0 + em(24.0), &fit(&format!("How {name} is put together"), x1 - x0 - em(400.0), type_size::LG), type_size::LG, Weight::Semibold, t.text_primary());
    let (sub, sub_c) = match (&c.check_error, c.checked) {
        (Some(e), _) => (format!("From atrium.toml · the check didn't run: {e}"), t.warning_fg()),
        (None, false) => ("From atrium.toml · checking with cargo atrium check…".into(), t.text_secondary()),
        (None, true) => ("From atrium.toml · checked by cargo atrium check".into(), t.text_secondary()),
    };
    text(ctx, x0 + em(50.0), y0 + em(46.0), &fit(&sub, x1 - x0 - em(400.0), type_size::XS), type_size::XS, Weight::Regular, sub_c);
    ctx.key(8600);
    Button::ghost("Check again").lead(phosphor::ARROW_CLOCKWISE).at(x1 - em(318.0), y0 + em(16.0)).width(em(134.0)).on_click(plain(q, Act::Check)).render(ctx);
    ctx.key(8601);
    let manifest = if c.project.is_empty() { "atrium.toml".to_string() } else { format!("{}/atrium.toml", c.project) };
    Button::secondary("Open atrium.toml").lead(phosphor::FILE_TEXT).at(x1 - em(176.0), y0 + em(16.0)).width(em(160.0))
        .on_click(plain(q, Act::Open(manifest))).render(ctx);
    let top = y0 + em(68.0);
    ctx.add(Node::Rect { rect: Rect::new(x0, top, x1 - x0, 1.0), fill: t.border_default(), radius: 0.0 });

    // Left: what the person is asked to allow.
    let lw = em(250.0);
    ctx.add(Node::Rect { rect: Rect::new(x0, top + 1.0, lw, y1 - top - 1.0), fill: t.bg_surface(), radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(x0 + lw - 1.0, top + 1.0, 1.0, y1 - top - 1.0), fill: t.border_default(), radius: 0.0 });
    let mut y = top + em(24.0);
    heading(ctx, x0 + em(16.0), y, "WHAT THE PERSON GRANTS", t);
    y += em(24.0);
    let grants: Vec<String> = if !c.consent.is_empty() { c.consent.clone() } else { c.holds.clone() };
    if grants.is_empty() {
        icon(ctx, x0 + em(16.0), y, phosphor::CHECK_CIRCLE, type_size::SM, t.success_fg());
        text(ctx, x0 + em(38.0), y, "Nothing: it asks for no access", type_size::SM, Weight::Regular, t.text_secondary());
        y += em(26.0);
    }
    for g in &grants {
        if y > y1 - em(30.0) {
            break;
        }
        icon(ctx, x0 + em(16.0), y, grant_icon(g), type_size::SM, t.accent_mark());
        text(ctx, x0 + em(38.0), y, &fit(g, lw - em(50.0), type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
        y += em(26.0);
    }
    if !c.roles.is_empty() && y < y1 - em(60.0) {
        y += em(12.0);
        heading(ctx, x0 + em(16.0), y, "ROLES IT SERVES", t);
        y += em(22.0);
        text(ctx, x0 + em(16.0), y, &fit(&c.roles.join(", "), lw - em(28.0), type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
        y += em(26.0);
    }
    if !c.embeds.is_empty() && y < y1 - em(60.0) {
        y += em(12.0);
        heading(ctx, x0 + em(16.0), y, "EMBEDS", t);
        y += em(22.0);
        text(ctx, x0 + em(16.0), y, &fit(&c.embeds.join(", "), lw - em(28.0), type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
    }

    // Right: the check.
    let rw = em(270.0);
    let rx = x1 - rw;
    ctx.add(Node::Rect { rect: Rect::new(rx, top + 1.0, rw, y1 - top - 1.0), fill: t.bg_surface(), radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(rx, top + 1.0, 1.0, y1 - top - 1.0), fill: t.border_default(), radius: 0.0 });
    let mut y = top + em(24.0);
    heading(ctx, rx + em(16.0), y, "CHECKS", t);
    y += em(24.0);
    let fw = rw - em(50.0);
    match (&c.check_error, c.checked) {
        (Some(e), _) => {
            icon(ctx, rx + em(16.0), y, phosphor::WARNING, type_size::SM, t.warning_fg());
            text(ctx, rx + em(38.0), y, &fit(e, fw, type_size::SM), type_size::SM, Weight::Regular, t.text_secondary());
        }
        (None, false) => text(ctx, rx + em(16.0), y, "Checking…", type_size::SM, Weight::Regular, t.text_secondary()),
        (None, true) if c.findings.is_empty() => {
            icon(ctx, rx + em(16.0), y, phosphor::CHECK_CIRCLE, type_size::SM, t.success_fg());
            text(ctx, rx + em(38.0), y, "Nothing to fix", type_size::SM, Weight::Regular, t.text_primary());
        }
        _ => {}
    }
    for fd in &c.findings {
        if y > y1 - em(40.0) {
            break;
        }
        let (ch, col) = match fd.level {
            Level::Error => (phosphor::WARNING_OCTAGON, t.danger_fg()),
            Level::Warning => (phosphor::WARNING, t.warning_fg()),
            Level::Note => (phosphor::CHECK_CIRCLE, t.info_fg()),
        };
        icon(ctx, rx + em(16.0), y, ch, type_size::SM, col);
        y += para(ctx, rx + em(38.0), y, &fd.message, fw, type_size::SM, Weight::Regular, t.text_primary(), 4);
        if let Some(fix) = &fd.fix {
            y += para(ctx, rx + em(38.0), y, fix, fw, type_size::XS, Weight::Regular, t.text_secondary(), 3);
        }
        y += em(8.0);
    }

    // Middle: the core, and under it the workers it calls.
    let mx = x0 + lw + em(28.0);
    let mw = rx - em(28.0) - mx;
    let core_crash = s.fuzz.as_ref().and_then(|f| f.crashes.iter().position(|x| x.in_core && x.frame.is_some()).map(|i| (i, &f.crashes[i], f)));
    let about = "Draws the window and talks to services. It should never parse what it reads.";
    let about_lines = wrap(about, mw - em(28.0), &style(font::SANS, type_size::XS, Weight::Regular, t.text_secondary()), 2).len().max(1);
    let extra = em(16.0) * (about_lines - 1) as f32;
    let ch = em(if core_crash.is_some() { 150.0 } else { 104.0 }) + extra + em(20.0) * c.holds.len().min(4) as f32;
    let core = Rect::new(mx, top + em(24.0), mw, ch);
    framed(ctx, core, t.bg_elevated(), t.accent_mark(), radius::MD);
    icon(ctx, core.x() + em(14.0), core.y() + em(22.0), phosphor::SHIELD_CHECK, type_size::MD, t.accent_mark());
    text(ctx, core.x() + em(38.0), core.y() + em(22.0), "Core", type_size::MD, Weight::Semibold, t.text_primary());
    let entry = if c.entry.is_empty() { "no entry".to_string() } else { c.entry.clone() };
    mono(ctx, core.x() + em(82.0), core.y() + em(22.0), &fitm(&format!("{entry} · holds the grants"), mw - em(96.0)), t.text_secondary());
    para(ctx, core.x() + em(14.0), core.y() + em(48.0), about, mw - em(28.0), type_size::XS, Weight::Regular, t.text_secondary(), 2);
    let mut y = core.y() + em(72.0) + extra;
    for h in c.holds.iter().take(4) {
        mono(ctx, core.x() + em(14.0), y, &fitm(h, mw - em(28.0)), t.text_primary());
        y += em(20.0);
    }
    if c.holds.is_empty() {
        text(ctx, core.x() + em(14.0), y, "Holds nothing", type_size::SM, Weight::Regular, t.text_tertiary());
        y += em(20.0);
    }
    if let Some((i, cr, f)) = core_crash {
        let about = f.targets.iter().find(|tg| tg.name == cr.target).map(|tg| tg.about.clone()).filter(|a| !a.is_empty()).unwrap_or_else(|| cr.target.clone());
        ctx.add(Node::Rect { rect: Rect::new(core.x() + em(8.0), y - em(4.0), mw - em(16.0), em(58.0)), fill: t.danger_fg().with_alpha(0.08), radius: radius::SM });
        icon(ctx, core.x() + em(16.0), y + em(10.0), phosphor::WARNING_OCTAGON, type_size::SM, t.danger_fg());
        text(ctx, core.x() + em(38.0), y + em(10.0), &fit(&format!("Parses {about}: fuzzing crashed it"), mw - em(60.0), type_size::SM), type_size::SM, Weight::Medium, t.danger_fg());
        ctx.key(8610);
        Button::secondary("Move it into a worker").lead(phosphor::PUZZLE_PIECE).at(core.x() + em(38.0), y + em(20.0)).width(em(196.0))
            .on_click(plain(q, Act::MoveToWorker(i))).render(ctx);
    }

    // The workers, two a row, joined to the core.
    let wy = core.y() + core.h() + em(40.0);
    let line_x = mx + mw / 2.0;
    ctx.add(Node::Rect { rect: Rect::new(line_x, core.y() + core.h(), 1.5, em(40.0)), fill: t.border_strong(), radius: 0.0 });
    let lbl = "messages";
    text(ctx, line_x + em(8.0), core.y() + core.h() + em(20.0), lbl, type_size::XXS, Weight::Regular, t.text_tertiary());
    if c.workers.is_empty() {
        let r = Rect::new(mx, wy, mw, em(56.0));
        framed(ctx, r, t.bg_canvas(), t.border_default(), radius::MD);
        icon(ctx, r.x() + em(14.0), r.y() + em(20.0), phosphor::PUZZLE_PIECE, type_size::MD, t.text_tertiary());
        text(ctx, r.x() + em(38.0), r.y() + em(20.0), "No workers: everything runs in the core", type_size::SM, Weight::Medium, t.text_secondary());
        text(ctx, r.x() + em(38.0), r.y() + em(40.0), &fit("A worker parses untrusted bytes in its own jail and holds nothing.", mw - em(50.0), type_size::XS), type_size::XS, Weight::Regular, t.text_tertiary());
    }
    let gap = em(16.0);
    let ww = (mw - gap) / 2.0;
    for (i, wk) in c.workers.iter().enumerate() {
        let r = Rect::new(mx + (i % 2) as f32 * (ww + gap), wy + (i / 2) as f32 * em(96.0), ww, em(82.0));
        if r.y() + r.h() > y1 - em(40.0) {
            break;
        }
        framed(ctx, r, t.bg_canvas(), t.border_strong(), radius::MD);
        icon(ctx, r.x() + em(12.0), r.y() + em(20.0), phosphor::PUZZLE_PIECE, type_size::MD, t.text_secondary());
        text(ctx, r.x() + em(36.0), r.y() + em(20.0), &fit(&wk.name, ww - em(130.0), type_size::SM), type_size::SM, Weight::Semibold, t.text_primary());
        if !wk.input.is_empty() {
            mono(ctx, r.x() + ww - em(12.0) - em(8.0) * (wk.input.len() + 7) as f32, r.y() + em(20.0), &format!("input: {}", wk.input), t.text_tertiary());
        }
        text(ctx, r.x() + em(12.0), r.y() + em(42.0), "Holds nothing: its own jail and uid", type_size::XS, Weight::Regular, t.text_secondary());
        let target = format!("worker:{}", wk.name);
        let (st, col, ch) = match &s.fuzz {
            Some(f) if f.crashes.iter().any(|x| x.target == target) => {
                let n = f.crashes.iter().filter(|x| x.target == target).count();
                (format!("Fuzzing found {n} crash{}: a bug, not a hole", if n == 1 { "" } else { "es" }), t.warning_fg(), phosphor::WARNING)
            }
            Some(f) if f.running.as_deref() == Some(target.as_str()) => (format!("Fuzzing now · {} runs", f.execs), t.success_fg(), phosphor::PULSE),
            Some(f) if f.targets.iter().any(|tg| tg.name == target) => ("A fuzz target · no crashes".into(), t.success_fg(), phosphor::CHECK_CIRCLE),
            _ => ("Not fuzzed yet".into(), t.text_tertiary(), phosphor::LIGHTNING),
        };
        icon(ctx, r.x() + em(12.0), r.y() + em(64.0), ch, type_size::XS, col);
        text(ctx, r.x() + em(30.0), r.y() + em(64.0), &fit(&st, ww - em(42.0), type_size::XS), type_size::XS, Weight::Regular, col);
    }
    // The key.
    let ky = y1 - em(20.0);
    let mut kx = mx;
    for (col, label) in [(t.accent_mark(), "Core: the only part with grants"), (t.border_strong(), "Worker: own jail, own uid, no access")] {
        ctx.add(Node::Rect { rect: Rect::new(kx, ky - em(5.0), em(10.0), em(10.0)), fill: col, radius: em(2.0) });
        text(ctx, kx + em(16.0), ky, label, type_size::XS, Weight::Regular, t.text_tertiary());
        kx += em(24.0) + em(6.2) * label.len() as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump() {
        let l = hex_lines(b"AA\n\0", 16);
        assert_eq!(l, vec![format!("00000000  {:<47}  AA..", "41 41 0a 00")]);
        assert_eq!(hex_lines(&[0u8; 17], 16).len(), 2);
        assert_eq!(hex_lines(&[0u8; 17], 8)[2], "00000010  00                       .");
        assert_eq!(frame_line("src/lib.rs:5"), Some(5));
        assert_eq!(frame_line("src/lib.rs"), None);
    }

    #[test]
    fn grants_get_icons() {
        assert_eq!(grant_icon("Pictures folder picked in the file picker"), phosphor::FOLDER);
        assert_eq!(grant_icon("network = [\"api.example.org\"]"), phosphor::GLOBE);
        assert_eq!(grant_icon("arca = true"), phosphor::KEY);
    }
}

/// An asset's size for people.
fn size_label(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1 << 20 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
    }
}

/// Icon tiles: each decoded size on a light and a dark ground, labelled.
fn icon_tiles(ctx: &mut Ctx, x: f32, y: f32, images: &[Result<pergola::node::Image, String>], t: Theme) -> f32 {
    let tile = em(64.0);
    let grounds = [Color::rgba(1.0, 1.0, 1.0, 1.0), Color::rgba(0.11, 0.11, 0.12, 1.0)];
    for (row, ground) in grounds.iter().enumerate() {
        for (i, n) in crate::assets::ICON_SIZES.iter().enumerate() {
            let r = Rect::new(x + i as f32 * (tile + em(10.0)), y + row as f32 * (tile + em(8.0)), tile, tile);
            framed(ctx, r, *ground, t.border_default(), radius::MD);
            match images.get(i) {
                Some(Ok(img)) => {
                    let sz = img.pt_size();
                    let (w, h) = (em(sz.w), em(sz.h));
                    ctx.add(Node::Image { rect: Rect::new(r.x() + (tile - w) / 2.0, r.y() + (tile - h) / 2.0, w, h), image: img.clone() });
                }
                Some(Err(_)) => icon(ctx, r.x() + tile / 2.0 - em(7.0), r.y() + tile / 2.0, phosphor::WARNING, type_size::MD, t.warning_fg()),
                None => {}
            }
            if row == 1 {
                text(ctx, r.x() + tile / 2.0 - em(8.0), r.y() + tile + em(14.0), &format!("{n} pt"), type_size::XXS, Weight::Regular, t.text_tertiary());
            }
        }
    }
    2.0 * tile + em(30.0)
}

/// The dock's verdict on an icon, as a line.
fn report_line(r: &crate::assets::IconReport) -> (String, bool) {
    let counts = format!("{} of {} shapes · {} of {} points · {} of {}",
        r.shapes, r.limits.0, r.points, r.limits.1, size_label(r.bytes), size_label(r.limits.2));
    match &r.problem {
        None => (format!("The dock draws it: {counts}"), true),
        Some(why) => (format!("The dock won't draw it: {why} ({counts})"), false),
    }
}

/// The Assets view: the project's `assets/` on the left, the chosen one
/// (or an import) on the right.
pub fn assets(ctx: &mut Ctx, s: &State, q: &Queue, area: Rect) {
    use crate::assets::{Importing, Kind};
    let t = ctx.theme;
    let (x0, y0, x1, y1) = (area.x(), area.y(), area.x() + area.w(), area.y() + area.h());
    let lw = fuzz_list_w();
    ctx.add(Node::Rect { rect: Rect::new(x0, y0, lw, area.h()), fill: t.bg_surface(), radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(x0 + lw - 1.0, y0, 1.0, area.h()), fill: t.border_default(), radius: 0.0 });
    text(ctx, x0 + em(16.0), y0 + em(26.0), "Assets", type_size::LG, Weight::Semibold, t.text_primary());
    ctx.key(8400);
    Button::secondary("Import SVG").lead(phosphor::TRAY_ARROW_DOWN).at(x0 + lw - em(136.0), y0 + em(10.0)).width(em(124.0))
        .on_click(plain(q, Act::ImportSvg)).render(ctx);
    let Some(a) = &s.assets else {
        text(ctx, x0 + em(16.0), y0 + em(60.0), "Reading assets/…", type_size::SM, Weight::Regular, t.text_secondary());
        return;
    };
    let mut y = y0 + em(56.0);
    if let Some(e) = &a.error {
        icon(ctx, x0 + em(16.0), y + em(10.0), phosphor::WARNING, type_size::SM, t.warning_fg());
        y += para(ctx, x0 + em(36.0), y + em(10.0), e, lw - em(48.0), type_size::XS, Weight::Regular, t.text_secondary(), 3);
    } else if a.files.is_empty() {
        y += para(ctx, x0 + em(16.0), y + em(10.0), "No assets yet. Pictures go in assets/ and icons in assets/icons/; Import SVG brings in an icon drawn in another app.",
                  lw - em(32.0), type_size::SM, Weight::Regular, t.text_secondary(), 5);
    }
    let mut last: Option<Kind> = None;
    for (i, f) in a.files.iter().enumerate() {
        if y > y1 - em(80.0) {
            break;
        }
        if last != Some(f.kind) {
            last = Some(f.kind);
            y += em(4.0);
            heading(ctx, x0 + em(16.0), y + em(6.0), match f.kind { Kind::Icon => "ICONS", Kind::Picture => "PICTURES", Kind::Other => "OTHER FILES" }, t);
            y += em(16.0);
        }
        ctx.key(8500 + i as u64);
        let r = Rect::new(x0 + em(8.0), y, lw - em(16.0), em(26.0));
        row(ctx, r, a.selected == i && a.importing.is_none(), t, q, Act::SelectAsset(i));
        let ch = match f.kind { Kind::Icon => phosphor::PAINT_BRUSH_BROAD, Kind::Picture => phosphor::IMAGE, Kind::Other => phosphor::FILE };
        icon(ctx, r.x() + em(8.0), r.y() + em(13.0), ch, type_size::SM, t.text_secondary());
        let size = size_label(f.bytes);
        let sw = Node::measure_text(&size, &style(font::SANS, type_size::XS, Weight::Regular, t.text_tertiary())).w;
        let shown = f.rel.strip_prefix("assets/icons/").or(f.rel.strip_prefix("assets/")).unwrap_or(&f.rel);
        text(ctx, r.x() + em(30.0), r.y() + em(13.0), &fit(shown, r.w() - em(46.0) - sw, type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
        text(ctx, r.x() + r.w() - sw - em(8.0), r.y() + em(13.0), &size, type_size::XS, Weight::Regular, t.text_tertiary());
        ctx.pop();
        y += em(28.0);
    }
    if a.truncated {
        text(ctx, x0 + em(16.0), y + em(10.0), &format!("Only the first {} are listed.", crate::assets::MAX_LISTED), type_size::XS, Weight::Regular, t.text_tertiary());
    }
    icon(ctx, x0 + em(16.0), y1 - em(40.0), phosphor::SHIELD_CHECK, type_size::SM, t.text_tertiary());
    text(ctx, x0 + em(36.0), y1 - em(40.0), "Previews come from the system's image", type_size::XS, Weight::Regular, t.text_tertiary());
    text(ctx, x0 + em(36.0), y1 - em(24.0), "decoder, in a jail of its own.", type_size::XS, Weight::Regular, t.text_tertiary());

    // The right pane.
    let pad = em(24.0);
    let cx = x0 + lw + pad;
    let cw = x1 - cx - pad;
    let mut y = y0 + em(36.0);
    if let Some(imp) = &a.importing {
        match imp {
            Importing::Choosing(list) => {
                text(ctx, cx, y, "Import an SVG as an icon", type_size::LG, Weight::Semibold, t.text_primary());
                y += em(26.0);
                para(ctx, cx, y, "Pick an SVG in this project. It becomes plain filled shapes the dock can draw; what it can't keep is listed before anything is written.",
                     cw, type_size::SM, Weight::Regular, t.text_secondary(), 3);
                // The list at a fixed place, under at most three lines.
                y = y0 + em(130.0);
                if list.is_empty() {
                    y += para(ctx, cx, y, "There's no SVG in this project outside assets/. Copy one into the project and try again.", cw, type_size::SM, Weight::Regular, t.text_secondary(), 3);
                }
                for (i, f) in list.iter().enumerate() {
                    if y > y1 - em(70.0) {
                        break;
                    }
                    ctx.key(8600 + i as u64);
                    let r = Rect::new(cx - em(8.0), y, cw.min(em(520.0)), em(26.0));
                    row(ctx, r, false, t, q, Act::ImportFrom(i));
                    icon(ctx, r.x() + em(8.0), r.y() + em(13.0), phosphor::FILE, type_size::SM, t.text_secondary());
                    text(ctx, r.x() + em(30.0), r.y() + em(13.0), &fit(f, r.w() - em(40.0), type_size::SM), type_size::SM, Weight::Regular, t.text_primary());
                    ctx.pop();
                    y += em(28.0);
                }
                ctx.key(8690);
                Button::ghost("Cancel").at(cx, y1 - em(52.0)).width(em(96.0)).on_click(plain(q, Act::ConfirmImport(false))).render(ctx);
            }
            Importing::Asking(src) => {
                text(ctx, cx, y, &fit(&format!("Converting {src}…"), cw, type_size::LG), type_size::LG, Weight::Semibold, t.text_primary());
            }
            Importing::Failed(why) => {
                icon(ctx, cx, y, phosphor::WARNING, type_size::LG, t.warning_fg());
                text(ctx, cx + em(30.0), y, "It can't be imported", type_size::LG, Weight::Semibold, t.text_primary());
                y += em(28.0);
                para(ctx, cx, y, why, cw, type_size::SM, Weight::Regular, t.text_secondary(), 6);
                ctx.key(8690);
                Button::ghost("Close").at(cx, y1 - em(52.0)).width(em(96.0)).on_click(plain(q, Act::ConfirmImport(false))).render(ctx);
            }
            Importing::Planned(p) => {
                text(ctx, cx, y, &fit(&format!("Import {} as {}", p.src, p.path), cw, type_size::LG), type_size::LG, Weight::Semibold, t.text_primary());
                y += em(30.0);
                let images = a.import_preview.clone().unwrap_or_default();
                y += icon_tiles(ctx, cx, y, &images, t);
                if let Some(Err(e)) = images.first() {
                    y += para(ctx, cx, y, &format!("No preview: {e}"), cw, type_size::XS, Weight::Regular, t.text_tertiary(), 2);
                }
                let (line, ok) = report_line(&p.report);
                icon(ctx, cx, y + em(8.0), if ok { phosphor::CHECK_CIRCLE } else { phosphor::WARNING }, type_size::SM, if ok { t.success_fg() } else { t.warning_fg() });
                y += para(ctx, cx + em(22.0), y + em(8.0), &line, cw - em(22.0), type_size::SM, Weight::Regular, t.text_primary(), 2) + em(6.0);
                if !p.converted.is_empty() {
                    y += para(ctx, cx, y + em(8.0), &format!("Converted: {}.", p.converted.join(", ")), cw, type_size::SM, Weight::Regular, t.text_secondary(), 3) + em(4.0);
                }
                if !p.dropped.is_empty() {
                    y += para(ctx, cx, y + em(8.0), &format!("Not kept: {}.", p.dropped.join(", ")), cw, type_size::SM, Weight::Medium, t.warning_fg(), 3) + em(4.0);
                }
                if p.replaces {
                    para(ctx, cx, y + em(8.0), &format!("This replaces the {} already there.", p.path), cw, type_size::SM, Weight::Regular, t.text_secondary(), 2);
                }
                if ok {
                    ctx.key(8691);
                    Button::primary("Import").lead(phosphor::TRAY_ARROW_DOWN).at(cx, y1 - em(52.0)).width(em(110.0)).on_click(plain(q, Act::ConfirmImport(true))).render(ctx);
                }
                ctx.key(8690);
                Button::ghost("Cancel").at(cx + if ok { em(120.0) } else { 0.0 }, y1 - em(52.0)).width(em(96.0)).on_click(plain(q, Act::ConfirmImport(false))).render(ctx);
            }
        }
        return;
    }
    let Some(f) = a.selected() else { return };
    let shown = f.rel.strip_prefix("assets/").unwrap_or(&f.rel);
    text(ctx, cx, y, &fit(shown, cw, type_size::LG), type_size::LG, Weight::Semibold, t.text_primary());
    y += em(22.0);
    let kind = match f.kind { Kind::Icon => "Icon", Kind::Picture => "Picture", Kind::Other => "File" };
    text(ctx, cx, y, &format!("{kind} · {} · atrium_assets::image(\"{}\", …)", size_label(f.bytes), f.name()), type_size::XS, Weight::Regular, t.text_tertiary());
    y += em(22.0);
    let preview = a.preview.as_ref().filter(|p| p.rel == f.rel);
    let images = preview.map(|p| p.images.clone()).unwrap_or_default();
    match f.kind {
        Kind::Icon => {
            y += icon_tiles(ctx, cx, y, &images, t);
            if let Some(Err(e)) = images.first() {
                y += para(ctx, cx, y, &format!("No preview: {e}"), cw, type_size::XS, Weight::Regular, t.text_tertiary(), 2);
            }
            match a.check.as_ref().filter(|(rel, _)| *rel == f.rel).map(|(_, r)| r) {
                Some(Ok(r)) => {
                    let (line, ok) = report_line(r);
                    icon(ctx, cx, y + em(8.0), if ok { phosphor::CHECK_CIRCLE } else { phosphor::WARNING }, type_size::SM, if ok { t.success_fg() } else { t.warning_fg() });
                    y += para(ctx, cx + em(22.0), y + em(8.0), &line, cw - em(22.0), type_size::SM, Weight::Regular, t.text_primary(), 2);
                }
                Some(Err(e)) => y += para(ctx, cx, y + em(8.0), &format!("Not checked: {e}"), cw, type_size::SM, Weight::Regular, t.text_tertiary(), 2),
                None => {
                    text(ctx, cx, y + em(8.0), "Checking it against the dock's limits…", type_size::SM, Weight::Regular, t.text_tertiary());
                    y += em(20.0);
                }
            }
        }
        Kind::Picture => {
            let (bw, bh) = (em(crate::assets::PICTURE_BOX.0 as f32).min(cw), em(crate::assets::PICTURE_BOX.1 as f32));
            let r = Rect::new(cx, y, bw, bh);
            framed(ctx, r, t.bg_surface(), t.border_default(), radius::MD);
            match images.first() {
                Some(Ok(img)) => {
                    let sz = img.pt_size();
                    let k = (bw / em(sz.w)).min(bh / em(sz.h)).min(1.0);
                    let (w, h) = (em(sz.w) * k, em(sz.h) * k);
                    ctx.add(Node::Image { rect: Rect::new(r.x() + (bw - w) / 2.0, r.y() + (bh - h) / 2.0, w, h), image: img.clone() });
                    let d = img.data();
                    text(ctx, cx, y + bh + em(16.0), &format!("Shown at {}×{} pixels", d.width, d.height), type_size::XS, Weight::Regular, t.text_tertiary());
                }
                Some(Err(e)) => {
                    para(ctx, r.x() + em(16.0), r.y() + em(24.0), &format!("No preview: {e}"), bw - em(32.0), type_size::SM, Weight::Regular, t.text_secondary(), 3);
                }
                None => text(ctx, r.x() + em(16.0), r.y() + em(24.0), "Decoding…", type_size::SM, Weight::Regular, t.text_tertiary()),
            }
            y += bh + em(28.0);
        }
        Kind::Other => {
            y += para(ctx, cx, y, "Not a picture the decoder opens. It is still bundled with the app.", cw, type_size::SM, Weight::Regular, t.text_secondary(), 2);
        }
    }
    y += em(18.0);
    heading(ctx, cx, y, "WHERE IT'S USED", t);
    y += em(18.0);
    match preview.and_then(|p| p.uses.as_ref()) {
        None => text(ctx, cx, y, "Searching the project…", type_size::SM, Weight::Regular, t.text_tertiary()),
        Some(u) if u.is_empty() => text(ctx, cx, y, &fit(&format!("Nothing in the project names \"{}\" yet.", f.name()), cw, type_size::SM), type_size::SM, Weight::Regular, t.text_tertiary()),
        Some(u) => {
            for (i, (file, line, l)) in u.iter().enumerate() {
                if y > y1 - em(30.0) {
                    break;
                }
                let rel = if s.project.is_empty() { file.clone() } else { format!("{}/{file}", s.project) };
                ctx.key(8700 + i as u64);
                let r = Rect::new(cx - em(8.0), y - em(12.0), cw + em(8.0), em(24.0));
                row(ctx, r, false, t, q, Act::Open(rel));
                let at = format!("{file}:{line}");
                let aw = Node::measure_text(&at, &style(font::SANS, type_size::XS, Weight::Medium, t.text_secondary())).w;
                text(ctx, cx, y, &at, type_size::XS, Weight::Medium, t.text_secondary());
                mono(ctx, cx + aw + em(12.0), y, &fitm(l, cw - aw - em(16.0)), t.text_primary());
                ctx.pop();
                y += em(26.0);
            }
        }
    }
}
