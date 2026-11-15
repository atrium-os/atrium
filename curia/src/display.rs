//! The Display page's model (curia.md §11): light or dark, the system text
//! size, and each monitor's UI size, resolution, place and mirroring. Owned
//! by forum-wm, asked over its settings socket (`display-settings`). Pure:
//! `main.rs` runs the calls and hands the answers to `apply`.

use std::time::{Duration, Instant};

use fresco_protocol::{WmDisplayMode, WmOutput, TEXT_SIZES, UI_SIZES, UI_SIZE_INTERNAL};

/// How long a new resolution stays without "Keep" before it goes back
/// (a mode the monitor shows black can't be clicked away from).
pub const KEEP_WAIT: Duration = Duration::from_secs(15);

/// Where one monitor sits next to another (forum-ctl's `Side`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    LeftOf,
    RightOf,
    Above,
    Below,
}

impl Place {
    pub const ALL: [Place; 4] = [Place::LeftOf, Place::RightOf, Place::Above, Place::Below];
    pub fn label(self) -> &'static str {
        match self {
            Place::LeftOf => "Left of",
            Place::RightOf => "Right of",
            Place::Above => "Above",
            Place::Below => "Below",
        }
    }
}

/// What the page asks forum-wm.
#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Refresh,
    Dark(bool),
    TextSize(u32),
    UiSize { output: u32, milli: u32 },
    Mode { width: u32, height: u32, refresh_mhz: u32 },
    Place { output: u32, place: Place, of: u32 },
    Mirror { output: u32, of: Option<u32> },
}

/// Everything the page shows, read in one go.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub dark: bool,
    pub text_size: u32,
    pub outputs: Vec<WmOutput>,
    pub modes: Vec<WmDisplayMode>,
    /// Index into `modes` of the lit one (`u32::MAX`: none).
    pub current: u32,
}

#[derive(Clone, Debug)]
pub enum Got {
    State(Result<Snapshot, String>),
    Done { what: Want, result: Result<(), String> },
}

/// A resolution just set, waiting for "Keep".
#[derive(Clone, Debug, PartialEq)]
pub struct Trial {
    pub was: (u32, u32, u32),
    pub until: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct DispPage {
    /// None: not asked yet; Some(Err): forum-wm can't be reached (why).
    pub state: Option<Result<Snapshot, String>>,
    /// The resolution list is open.
    pub picking_mode: bool,
    pub trial: Option<Trial>,
    /// The last thing that went wrong, shown at the foot.
    pub notice: Option<String>,
    pub wants: Vec<Want>,
}

/// "Smaller", "Default", "Larger", then percentages.
pub fn ui_size_label(milli: u32) -> String {
    match milli {
        875 => "Smaller".into(),
        1000 => "Default".into(),
        1125 => "Larger".into(),
        m => format!("{}%", m / 10),
    }
}

/// A mode in words: "2560 × 1440 · 59.95 Hz".
pub fn mode_label(m: &WmDisplayMode) -> String {
    let hz = m.refresh_mhz as f64 / 1000.0;
    let hz = if (hz - hz.round()).abs() < 0.005 { format!("{}", hz.round() as u32) } else { format!("{hz:.2}") };
    format!("{} × {} · {hz} Hz", m.width, m.height)
}

/// A monitor's name: "Display 1", its diagonal when the EDID gives one, and
/// "built-in" for a laptop panel.
pub fn output_name(i: usize, o: &WmOutput) -> String {
    let mut s = format!("Display {}", i + 1);
    let mut extra = Vec::new();
    if o.internal {
        extra.push("built-in".to_string());
    }
    if o.width_mm > 0 && o.height_mm > 0 {
        let d = ((o.width_mm as f64).powi(2) + (o.height_mm as f64).powi(2)).sqrt() / 25.4;
        extra.push(format!("{}″", d.round() as u32));
    }
    if !extra.is_empty() {
        s.push_str(&format!(" ({})", extra.join(", ")));
    }
    s
}

/// The UI size a monitor starts at.
pub fn default_ui_size(o: &WmOutput) -> u32 {
    if o.internal { UI_SIZE_INTERNAL } else { 1000 }
}

impl DispPage {
    pub fn new() -> Self {
        DispPage { wants: vec![Want::Refresh], ..Default::default() }
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.state.as_ref().and_then(|s| s.as_ref().ok())
    }

    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    pub fn apply(&mut self, g: Got) {
        match g {
            Got::State(Ok(s)) => {
                self.state = Some(Ok(s));
            }
            // A failed read keeps nothing stale on screen: the page says why.
            Got::State(Err(e)) => {
                self.state = Some(Err(e));
                self.picking_mode = false;
            }
            Got::Done { what, result } => {
                match result {
                    Ok(()) => self.notice = None,
                    Err(e) => {
                        self.notice = Some(format!("{}: {e}", match &what {
                            Want::Dark(_) => "Light or dark",
                            Want::TextSize(_) => "Text size",
                            Want::UiSize { .. } => "UI size",
                            Want::Mode { .. } => "Resolution",
                            Want::Place { .. } => "Arrangement",
                            Want::Mirror { .. } => "Mirroring",
                            Want::Refresh => "Display settings",
                        }));
                        // A refused mode never went on, so there is nothing to keep.
                        if matches!(what, Want::Mode { .. }) {
                            self.trial = None;
                        }
                    }
                }
                self.wants.push(Want::Refresh);
            }
        }
    }

    fn act(&mut self, w: Want) {
        if self.snapshot().is_some() {
            self.wants.push(w);
        }
    }

    pub fn set_dark(&mut self, dark: bool) {
        if self.snapshot().is_some_and(|s| s.dark != dark) {
            self.act(Want::Dark(dark));
        }
    }

    pub fn set_text_size(&mut self, milli: u32) {
        if TEXT_SIZES.contains(&milli) && self.snapshot().is_some_and(|s| s.text_size != milli) {
            self.act(Want::TextSize(milli));
        }
    }

    pub fn set_ui_size(&mut self, output: u32, milli: u32) {
        let Some(o) = self.snapshot().and_then(|s| s.outputs.iter().find(|o| o.output == output)) else { return };
        if !UI_SIZES.contains(&milli) || o.ui_size_milli == milli {
            return;
        }
        // The monitor's own default is stored as "not set", so a new
        // monitor default (a laptop panel at Smaller) still applies.
        let m = if milli == default_ui_size(o) { 0 } else { milli };
        self.act(Want::UiSize { output, milli: m });
    }

    /// The modes to offer: one per size (the best refresh), largest first.
    pub fn mode_choices(&self) -> Vec<WmDisplayMode> {
        let Some(s) = self.snapshot() else { return Vec::new() };
        let mut v: Vec<WmDisplayMode> = Vec::new();
        for m in &s.modes {
            match v.iter_mut().find(|x| (x.width, x.height) == (m.width, m.height)) {
                Some(x) if m.refresh_mhz > x.refresh_mhz => x.refresh_mhz = m.refresh_mhz,
                Some(x) => x.preferred |= m.preferred,
                None => v.push(*m),
            }
        }
        v.sort_by(|a, b| (b.width * b.height, b.refresh_mhz).cmp(&(a.width * a.height, a.refresh_mhz)));
        v
    }

    pub fn current_mode(&self) -> Option<WmDisplayMode> {
        let s = self.snapshot()?;
        s.modes.get(s.current as usize).copied()
    }

    pub fn toggle_modes(&mut self) {
        self.picking_mode = !self.picking_mode && self.snapshot().is_some() && self.trial.is_none();
    }

    /// Switch to `m`, and go back unless "Keep" is pressed within
    /// [`KEEP_WAIT`].
    pub fn pick_mode(&mut self, m: WmDisplayMode, now: Instant) {
        self.picking_mode = false;
        let Some(cur) = self.current_mode() else { return };
        if (cur.width, cur.height, cur.refresh_mhz) == (m.width, m.height, m.refresh_mhz) || self.trial.is_some() {
            return;
        }
        self.trial = Some(Trial { was: (cur.width, cur.height, cur.refresh_mhz), until: now + KEEP_WAIT });
        self.act(Want::Mode { width: m.width, height: m.height, refresh_mhz: m.refresh_mhz });
    }

    pub fn keep_mode(&mut self) {
        self.trial = None;
    }

    pub fn revert_mode(&mut self) {
        if let Some(t) = self.trial.take() {
            let (width, height, refresh_mhz) = t.was;
            self.act(Want::Mode { width, height, refresh_mhz });
        }
    }

    /// Seconds left to keep a new resolution.
    pub fn trial_left(&self, now: Instant) -> Option<u64> {
        self.trial.as_ref().map(|t| t.until.saturating_duration_since(now).as_secs_f32().ceil() as u64)
    }

    /// Time passes: an unkept resolution goes back. True when something changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.trial.as_ref().is_some_and(|t| now >= t.until) {
            self.revert_mode();
            self.notice = Some("The resolution went back, as it wasn't kept".into());
            return true;
        }
        self.trial.is_some()
    }

    pub fn place(&mut self, output: u32, place: Place, of: u32) {
        if output != of {
            self.act(Want::Place { output, place, of });
        }
    }

    pub fn mirror(&mut self, output: u32, of: Option<u32>) {
        if of != Some(output) {
            self.act(Want::Mirror { output, of });
        }
    }

    /// Which output a mirroring one shows, by output number.
    pub fn mirrors(&self, o: &WmOutput) -> Option<u32> {
        if o.mirror_of == 0 {
            return None;
        }
        self.snapshot()?.outputs.iter().find(|x| x.identity == o.mirror_of).map(|x| x.output)
    }

    /// Where `o` sits next to `of`, from their rects, if they touch.
    pub fn placed(o: &WmOutput, of: &WmOutput) -> Option<Place> {
        let (a, b) = (&o.rect, &of.rect);
        let overlap_y = a.y < b.y + b.h && b.y < a.y + a.h;
        let overlap_x = a.x < b.x + b.w && b.x < a.x + a.w;
        if a.x + a.w == b.x && overlap_y {
            Some(Place::LeftOf)
        } else if b.x + b.w == a.x && overlap_y {
            Some(Place::RightOf)
        } else if a.y + a.h == b.y && overlap_x {
            Some(Place::Above)
        } else if b.y + b.h == a.y && overlap_x {
            Some(Place::Below)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresco_protocol::WmRect;

    fn out(n: u32, x: i32, internal: bool) -> WmOutput {
        WmOutput {
            output: n,
            identity: 100 + n as u64,
            rect: WmRect { x, y: 0, w: 1920, h: 1080 },
            refresh_mhz: 60000,
            present: true,
            scale_milli: 1000,
            auto_scale_milli: 1000,
            ui_size_milli: if internal { 875 } else { 1000 },
            ui_size_set: false,
            internal,
            mirror_of: 0,
            width_mm: 600,
            height_mm: 340,
            flapping: false,
        }
    }

    fn mode(w: u32, h: u32, r: u32) -> WmDisplayMode {
        WmDisplayMode { width: w, height: h, refresh_mhz: r, preferred: false }
    }

    fn page() -> DispPage {
        let mut p = DispPage::new();
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        p.apply(Got::State(Ok(Snapshot {
            dark: false,
            text_size: 1000,
            outputs: vec![out(0, 0, true), out(1, 1920, false)],
            modes: vec![mode(1920, 1080, 60000), mode(1920, 1080, 59940), mode(1280, 720, 60000), mode(2560, 1440, 59951)],
            current: 0,
        })));
        p
    }

    #[test]
    fn settings_go_to_forum_wm_only_when_they_change() {
        let mut p = page();
        p.set_dark(false);
        p.set_text_size(1000);
        p.set_text_size(1234);
        assert!(p.take_wants().is_empty());
        p.set_dark(true);
        p.set_text_size(1300);
        assert_eq!(p.take_wants(), vec![Want::Dark(true), Want::TextSize(1300)]);
    }

    #[test]
    fn a_monitors_own_default_is_sent_as_unset() {
        let mut p = page();
        p.set_ui_size(1, 1250);
        p.set_ui_size(0, 1000); // the laptop panel's Default is not its default
        p.set_ui_size(1, 1000); // already
        p.set_ui_size(1, 1100); // not a UI size
        assert_eq!(p.take_wants(), vec![Want::UiSize { output: 1, milli: 1250 }, Want::UiSize { output: 0, milli: 1000 }]);
        if let Some(Ok(s)) = &mut p.state {
            s.outputs[0].ui_size_milli = 1250;
        }
        p.set_ui_size(0, 875);
        assert_eq!(p.take_wants(), vec![Want::UiSize { output: 0, milli: 0 }]);
    }

    #[test]
    fn modes_are_offered_once_per_size_largest_first() {
        let p = page();
        let c: Vec<_> = p.mode_choices().iter().map(mode_label).collect();
        assert_eq!(c, ["2560 × 1440 · 59.95 Hz", "1920 × 1080 · 60 Hz", "1280 × 720 · 60 Hz"]);
    }

    #[test]
    fn a_new_resolution_goes_back_unless_kept() {
        let t0 = Instant::now();
        let mut p = page();
        p.pick_mode(mode(1280, 720, 60000), t0);
        assert_eq!(p.take_wants(), vec![Want::Mode { width: 1280, height: 720, refresh_mhz: 60000 }]);
        assert_eq!(p.trial_left(t0 + Duration::from_secs(5)), Some(10));
        // A second pick while one waits is ignored.
        p.pick_mode(mode(2560, 1440, 59951), t0);
        assert!(p.take_wants().is_empty());
        assert!(p.tick(t0 + Duration::from_secs(14)));
        assert!(p.take_wants().is_empty());
        p.tick(t0 + KEEP_WAIT);
        assert_eq!(p.take_wants(), vec![Want::Mode { width: 1920, height: 1080, refresh_mhz: 60000 }]);
        assert!(p.trial.is_none() && p.notice.is_some());
        // Kept: nothing goes back.
        p.pick_mode(mode(1280, 720, 60000), t0);
        p.take_wants();
        p.keep_mode();
        assert!(!p.tick(t0 + KEEP_WAIT * 2));
        assert!(p.take_wants().is_empty());
        // Refused: nothing to keep.
        p.pick_mode(mode(1280, 720, 60000), t0);
        let w = p.take_wants().remove(0);
        p.apply(Got::Done { what: w, result: Err("no such mode".into()) });
        assert!(p.trial.is_none());
        assert_eq!(p.notice.as_deref(), Some("Resolution: no such mode"));
    }

    #[test]
    fn nothing_is_sent_while_forum_wm_is_away() {
        let mut p = page();
        p.apply(Got::State(Err("forum-wm is not running".into())));
        p.set_dark(true);
        p.set_text_size(1500);
        p.place(1, Place::LeftOf, 0);
        assert!(p.take_wants().is_empty());
    }

    #[test]
    fn places_and_names() {
        let p = page();
        let s = p.snapshot().unwrap();
        assert_eq!(DispPage::placed(&s.outputs[1], &s.outputs[0]), Some(Place::RightOf));
        assert_eq!(DispPage::placed(&s.outputs[0], &s.outputs[1]), Some(Place::LeftOf));
        assert_eq!(output_name(0, &s.outputs[0]), "Display 1 (built-in, 27″)");
        assert_eq!(ui_size_label(1500), "150%");
    }
}
