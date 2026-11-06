//! Drag and drop between windows (docs/spec/tabula.md §4): frescod's half.
//!
//! The payload stays with Tabula. frescod knows which press is held, lets
//! the window it went to start a drag while it is (`begin`), follows the
//! pointer over windows (`EV_DRAG_ENTER/OVER/LEAVE`), and on release mints
//! a `DROP` token for the window under the pointer, bound to the drag, and
//! sends it `EV_DROP`. The source gets `EV_DRAG_END`.

use std::sync::{Arc, Mutex};

use fresco_protocol::DragBegin;
use fresco_scene_server::window::{Compositor, DisplayEvent as E, DragPhase, MINT_HERE};

/// The press being held: a button press that got a token.
#[derive(Debug)]
struct Held {
    window_id: u32,
    token: [u8; 16],
    button: u8,
}

#[derive(Debug)]
struct Drag {
    source: u32,
    drag_id: u64,
    formats: Vec<String>,
    button: u8,
    /// The window the drag is over (0: none).
    over: u32,
}

#[derive(Default, Debug)]
struct State {
    held: Option<Held>,
    drag: Option<Drag>,
}

static STATE: Mutex<State> = Mutex::new(State { held: None, drag: None });

/// The window under the pointer and the pointer in its coordinates.
fn under(comp: &Mutex<Compositor>) -> (u32, f32, f32) {
    let g = comp.lock().unwrap();
    let (x, y) = g.cursor;
    let (t, (lx, ly)) = g.pointer_target(x, y);
    (t as u32, lx, ly)
}

/// Start a drag for the client `client_id`: `b.activation` must be the
/// press still held, in `b.window_id`, a window of that client's, which has
/// focus. The token is not spent (Tabula spent it on `DRAG_BEGIN`).
pub fn begin(comp: &Mutex<Compositor>, client_id: u8, b: &DragBegin) -> Result<(), &'static str> {
    if b.drag_id == 0 || b.activation == [0; 16] || !fresco_protocol::drag::formats_ok(&b.formats) {
        return Err("bad drag");
    }
    let mut s = STATE.lock().unwrap();
    if s.drag.is_some() {
        return Err("a drag is already on");
    }
    let held = s.held.as_ref().ok_or("no press is held")?;
    if held.window_id != b.window_id || held.token != b.activation {
        return Err("not the press being held");
    }
    let owner = u16::try_from(b.window_id).ok().and_then(|id| comp.lock().unwrap().windows.get(&id).map(|w| w.owner));
    if owner != Some(client_id as _) {
        return Err("not the client's window");
    }
    if crate::activation::focused_window(comp) != b.window_id {
        return Err("window not focused");
    }
    let button = held.button;
    s.drag = Some(Drag { source: b.window_id, drag_id: b.drag_id, formats: b.formats.clone(), button, over: 0 });
    Ok(())
}

/// Follow input (after its token is minted) and return the drag events it
/// causes, to fan out after it. `uid(window)` is the uid of a window's app.
pub fn observe(ev: &E, comp: &Arc<Mutex<Compositor>>, uid: impl Fn(u32) -> Option<u32>) -> Vec<E> {
    let mut s = STATE.lock().unwrap();
    let mut out = Vec::new();
    match *ev {
        E::InputPointerButton { window_id, button, pressed: true, activation, .. } => {
            if s.drag.is_none() && activation != [0; 16] && activation != MINT_HERE {
                s.held = Some(Held { window_id, token: activation, button });
            }
        }
        E::InputPointerButton { button, pressed: false, .. } => {
            if s.held.as_ref().is_some_and(|h| h.button == button) {
                s.held = None;
            }
            if s.drag.as_ref().is_some_and(|d| d.button == button) {
                let d = s.drag.take().unwrap();
                let (t, x, y) = under(comp);
                if d.over != 0 && d.over != t {
                    out.push(E::Drag { phase: DragPhase::Leave, window_id: d.over, drag_id: d.drag_id, x: 0.0, y: 0.0, formats: Vec::new() });
                }
                let to = (t != 0).then(|| uid(t)).flatten();
                if let Some(u) = to {
                    let activation = crate::activation::mint_drop(t, u, d.drag_id);
                    out.push(E::Drop { window_id: t, x, y, activation });
                }
                out.push(E::DragEnd { window_id: d.source, drag_id: d.drag_id, dropped: to.is_some() });
            }
        }
        E::InputPointerMotion { .. } => {
            if let Some(d) = s.drag.as_mut() {
                let (t, x, y) = under(comp);
                if t != d.over {
                    if d.over != 0 {
                        out.push(E::Drag { phase: DragPhase::Leave, window_id: d.over, drag_id: d.drag_id, x: 0.0, y: 0.0, formats: Vec::new() });
                    }
                    if t != 0 {
                        out.push(E::Drag { phase: DragPhase::Enter, window_id: t, drag_id: d.drag_id, x, y, formats: d.formats.clone() });
                    }
                    d.over = t;
                } else if t != 0 {
                    out.push(E::Drag { phase: DragPhase::Over, window_id: t, drag_id: d.drag_id, x, y, formats: Vec::new() });
                }
            }
        }
        E::WindowDestroyed { window_id } => {
            if s.drag.as_ref().is_some_and(|d| d.source == window_id) {
                s.drag = None;
            }
            if let Some(d) = s.drag.as_mut().filter(|d| d.over == window_id) {
                d.over = 0;
            }
            if s.held.as_ref().is_some_and(|h| h.window_id == window_id) {
                s.held = None;
            }
        }
        _ => {}
    }
    out
}


/// Hook drags into the event fan-out and the clients' `OP_DRAG_BEGIN`.
pub fn install(comp: Arc<Mutex<Compositor>>) {
    use crate::socket_server as ss;
    let _ = ss::OBSERVE.set(|ev, comp| observe(ev, comp, |w| ss::window_owner_uid(w, comp)));
    let _ = ss::DRAG_BEGIN.set(Box::new(move |client, b| match begin(&comp, client, b) {
        Ok(()) => true,
        Err(e) => {
            log::info!("frescod: client {client}: drag {} refused: {e}", b.drag_id);
            false
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresco_protocol::activation::{kind, purpose, ConsumeResult};

    fn press(window_id: u32, pressed: bool, token: [u8; 16]) -> E {
        E::InputPointerButton { window_id, x: 1.0, y: 1.0, button: 1, pressed, modifiers: 0, activation: token }
    }
    fn motion(comp: &Mutex<Compositor>, x: f32, y: f32) -> E {
        comp.lock().unwrap().cursor = (x, y);
        E::InputPointerMotion { window_id: 0, x, y }
    }
    fn begin_with(comp: &Mutex<Compositor>, client: u8, window_id: u32, token: [u8; 16]) -> Result<(), &'static str> {
        begin(comp, client, &DragBegin { window_id, drag_id: 77, activation: token, formats: vec!["text/plain".into()] })
    }

    /// One test: the drag state is one per frescod.
    #[test]
    fn a_drag_follows_the_pointer_and_drops_a_token_for_the_target() {
        let comp = Arc::new(Mutex::new(Compositor::new()));
        let (src, dst) = {
            let mut c = comp.lock().unwrap();
            let a = c.create(1, (100, 100));
            let b = c.create(2, (100, 100));
            c.windows.get_mut(&b).unwrap().pos = (200.0, 0.0);
            c.set_focus_no_raise(Some(a));
            (a as u32, b as u32)
        };
        let uid = |w: u32| Some(1000 + w);
        let t = [3; 16];
        assert!(begin_with(&comp, 1, src, t).is_err(), "no press held");
        observe(&press(src, true, t), &comp, uid);
        assert!(begin_with(&comp, 1, src, [4; 16]).is_err(), "another token");
        assert!(begin_with(&comp, 2, src, t).is_err(), "another client's window");
        assert!(begin_with(&comp, 1, src, t).is_ok());
        assert!(begin_with(&comp, 1, src, t).is_err(), "one drag at a time");

        let ev = observe(&motion(&comp, 50.0, 50.0), &comp, uid);
        assert!(matches!(&ev[..], [E::Drag { phase: DragPhase::Enter, window_id, formats, .. }] if *window_id == src && formats.len() == 1));
        let ev = observe(&motion(&comp, 250.0, 40.0), &comp, uid);
        assert!(matches!(&ev[..], [E::Drag { phase: DragPhase::Leave, window_id: a, .. }, E::Drag { phase: DragPhase::Enter, window_id: b, .. }] if *a == src && *b == dst));
        let ev = observe(&motion(&comp, 260.0, 40.0), &comp, uid);
        assert!(matches!(&ev[..], [E::Drag { phase: DragPhase::Over, x, .. }] if *x == 60.0));

        let ev = observe(&press(src, false, [0; 16]), &comp, uid);
        let [E::Drop { window_id, activation, .. }, E::DragEnd { window_id: s, dropped: true, drag_id: 77 }] = &ev[..] else { panic!("{ev:?}") };
        assert_eq!((*window_id, *s), (dst, src));
        let mut table = crate::activation::table().lock().unwrap();
        let now = crate::activation::now_ns();
        let got = table.consume(now, activation, 1000 + dst, purpose::DROP, |_| false);
        assert!(matches!(got, ConsumeResult::Ok { kind: kind::DROP, drag_id: 77, .. }), "{got:?}: a drop needs no focus");
        drop(table);

        // Released over no window: dropped on nothing.
        observe(&press(src, true, t), &comp, uid);
        assert!(begin_with(&comp, 1, src, t).is_ok());
        let ev = observe(&motion(&comp, 150.0, 300.0), &comp, uid);
        assert!(ev.is_empty());
        let ev = observe(&press(src, false, [0; 16]), &comp, uid);
        assert!(matches!(&ev[..], [E::DragEnd { dropped: false, .. }]), "{ev:?}");
        assert!(begin_with(&comp, 1, src, t).is_err(), "the press was let go");
    }
}
