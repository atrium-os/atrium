//! The Sound page's model (curia.md §12): the master volume, each app that
//! has a stream open, and the outputs (which one plays, balance, speakers,
//! headphone surround, where the volume is applied). Owned by choragusd,
//! asked over its volume-control socket (`audio-control`; lyra-v1.md §7).
//! Pure: `main.rs` keeps the connection and hands what arrives to `apply`.

use lyra_protocol::wire::{AppEntry, FromControl, Output, ToControl, VolumeMode};

/// Speaker layouts choragusd takes.
pub const LAYOUTS: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1"), (8, "7.1")];

#[derive(Clone, Debug, Default)]
pub struct SoundPage {
    /// None: not connected yet; Some(Err): why choragusd can't be reached.
    pub link: Option<Result<(), String>>,
    pub master: f32,
    pub master_muted: bool,
    pub apps: Vec<AppEntry>,
    pub current: u32,
    pub outputs: Vec<Output>,
    /// Apps and outputs have both been heard since connecting.
    pub heard: (bool, bool),
    pub wants: Vec<ToControl>,
}

/// "Automatic", "Fixed (line out)", … for an output's volume setting.
pub fn volume_mode_label(m: Option<VolumeMode>) -> &'static str {
    match m {
        None => "Automatic",
        Some(VolumeMode::Digital) => "In the mix",
        Some(VolumeMode::Hardware) => "Device's own",
        Some(VolumeMode::Fixed) => "Fixed (line out)",
    }
}

/// "42%" for a linear level (what the bar shows too).
pub fn percent(v: f32) -> String {
    format!("{}%", (v.clamp(0.0, 1.0) * 100.0).round() as u32)
}

/// An app's name for the list: its id's last part ("org.atrium.player" →
/// "player"), and the stream's label when it says more.
pub fn app_name(a: &AppEntry) -> String {
    let short = a.app_id.rsplit('.').next().unwrap_or(&a.app_id);
    let mut s = short[..1].to_uppercase() + &short[1..];
    if !a.label.is_empty() && !a.label.eq_ignore_ascii_case(short) {
        s.push_str(&format!(" · {}", a.label));
    }
    s
}

fn clamp(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

impl SoundPage {
    pub fn take_wants(&mut self) -> Vec<ToControl> {
        std::mem::take(&mut self.wants)
    }

    pub fn ready(&self) -> bool {
        matches!(self.link, Some(Ok(()))) && self.heard.0 && self.heard.1
    }

    /// Connected: ask for everything.
    pub fn connected(&mut self) {
        self.link = Some(Ok(()));
        self.heard = (false, false);
        self.wants.clear();
    }

    /// The connection went (or never came): say why; nothing stale stays.
    pub fn lost(&mut self, why: String) {
        *self = SoundPage { link: Some(Err(why)), ..Default::default() };
    }

    pub fn apply(&mut self, m: FromControl) {
        match m {
            FromControl::Apps { master, master_muted, apps } => {
                self.master = clamp(master);
                self.master_muted = master_muted;
                self.apps = apps;
                self.heard.0 = true;
            }
            FromControl::Outputs { current, outputs } => {
                self.current = current;
                self.outputs = outputs;
                self.heard.1 = true;
            }
            FromControl::Welcome => {}
            FromControl::Denied { code } => self.link = Some(Err(format!("choragusd refused ({code:?})"))),
        }
    }

    fn act(&mut self, m: ToControl) {
        if self.ready() {
            self.wants.push(m);
        }
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.outputs.iter().find(|o| o.id == self.current)
    }

    /// The master level, shown at once (choragusd's `Apps` confirms it).
    pub fn set_master(&mut self, v: f32) {
        let v = clamp(v);
        if self.ready() && (v - self.master).abs() > 0.004 {
            self.master = v;
            self.act(ToControl::SetMaster { volume: v });
        }
    }

    pub fn toggle_master_mute(&mut self) {
        if self.ready() {
            self.master_muted = !self.master_muted;
            self.act(ToControl::SetMasterMute { muted: self.master_muted });
        }
    }

    pub fn set_app(&mut self, app_id: &str, v: f32) {
        let v = clamp(v);
        if !self.ready() {
            return;
        }
        if let Some(a) = self.apps.iter_mut().find(|a| a.app_id == app_id) {
            if (a.volume - v).abs() > 0.004 {
                a.volume = v;
                self.wants.push(ToControl::SetAppVolume { app_id: app_id.into(), volume: v });
            }
        }
    }

    pub fn toggle_app_mute(&mut self, app_id: &str) {
        if !self.ready() {
            return;
        }
        if let Some(a) = self.apps.iter_mut().find(|a| a.app_id == app_id) {
            a.muted = !a.muted;
            self.wants.push(ToControl::SetAppMute { app_id: app_id.into(), muted: a.muted });
        }
    }

    pub fn toggle_app_spatial(&mut self, app_id: &str) {
        if !self.ready() {
            return;
        }
        if let Some(a) = self.apps.iter_mut().find(|a| a.app_id == app_id) {
            a.spatial = !a.spatial;
            self.wants.push(ToControl::SetAppSpatial { app_id: app_id.into(), on: a.spatial });
        }
    }

    pub fn pick_output(&mut self, id: u32) {
        if id != self.current && self.outputs.iter().any(|o| o.id == id) {
            self.act(ToControl::SetOutput { output: id });
        }
    }

    /// Balance, −1 to 1; within 2% of the middle is the middle.
    pub fn set_balance(&mut self, b: f32) {
        let b = if b.is_nan() { 0.0 } else if b.abs() < 0.02 { 0.0 } else { b.clamp(-1.0, 1.0) };
        let id = self.current;
        if let Some(o) = self.outputs.iter_mut().find(|o| o.id == id) {
            if (o.balance - b).abs() > 0.004 && matches!(self.link, Some(Ok(()))) {
                o.balance = b;
                self.wants.push(ToControl::SetBalance { output: id, balance: b });
            }
        }
    }

    pub fn set_layout(&mut self, channels: u8) {
        let id = self.current;
        if LAYOUTS.iter().any(|(c, _)| *c == channels) && self.current_output().is_some_and(|o| o.channels != channels) {
            self.act(ToControl::SetLayout { output: id, channels });
        }
    }

    /// Headphone surround: only offered on a stereo layout.
    pub fn toggle_spatial(&mut self) {
        let Some(o) = self.current_output() else { return };
        if o.channels == 2 {
            let (id, on) = (o.id, !o.spatial);
            self.act(ToControl::SetSpatial { output: id, on });
        }
    }

    /// The volume settings an output offers ("Device's own" only with a
    /// control of its own).
    pub fn volume_modes(o: &Output) -> Vec<Option<VolumeMode>> {
        let mut v = vec![None, Some(VolumeMode::Digital)];
        if o.hw_volume {
            v.push(Some(VolumeMode::Hardware));
        }
        v.push(Some(VolumeMode::Fixed));
        v
    }

    pub fn set_volume_mode(&mut self, m: Option<VolumeMode>) {
        let Some(o) = self.current_output() else { return };
        if o.volume_setting != m && Self::volume_modes(o).contains(&m) {
            let id = o.id;
            self.act(ToControl::SetVolumeMode { output: id, mode: m });
        }
    }

    /// The master can't be changed while the output's volume is fixed (an
    /// amplifier's knob is the volume).
    pub fn master_fixed(&self) -> bool {
        self.current_output().is_some_and(|o| o.volume == VolumeMode::Fixed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyra_protocol::wire::Role;

    fn out(id: u32, name: &str, hw: bool) -> Output {
        Output { id, name: name.into(), volume: VolumeMode::Digital, volume_setting: None, hw_volume: hw, balance: 0.0, channels: 2, spatial: false }
    }

    fn page() -> SoundPage {
        let mut p = SoundPage::default();
        p.connected();
        let app = AppEntry { app_id: "org.atrium.player".into(), label: "Video".into(), role: Role::Media, volume: 0.8, muted: false, streams: 1, spatial: true };
        p.apply(FromControl::Apps { master: 0.5, master_muted: false, apps: vec![app] });
        assert!(!p.ready());
        p.apply(FromControl::Outputs { current: 1, outputs: vec![out(1, "Speakers", false), out(2, "USB DAC", true)] });
        assert!(p.ready());
        p
    }

    #[test]
    fn levels_go_to_choragusd_and_show_at_once() {
        let mut p = page();
        p.set_master(0.5);
        p.set_master(f32::NAN);
        p.set_app("org.atrium.player", 1.7);
        p.set_app("nobody", 0.1);
        p.toggle_app_mute("org.atrium.player");
        assert_eq!(p.master, 0.0);
        assert_eq!(p.apps[0].volume, 1.0);
        assert_eq!(p.take_wants(), vec![
            ToControl::SetMaster { volume: 0.0 },
            ToControl::SetAppVolume { app_id: "org.atrium.player".into(), volume: 1.0 },
            ToControl::SetAppMute { app_id: "org.atrium.player".into(), muted: true },
        ]);
    }

    #[test]
    fn outputs_balance_layout_surround_and_volume_mode() {
        let mut p = page();
        p.pick_output(1);
        p.pick_output(9);
        p.pick_output(2);
        p.set_balance(0.01);
        p.set_balance(-0.5);
        p.set_layout(2);
        p.set_layout(6);
        p.set_layout(4);
        p.toggle_spatial();
        p.set_volume_mode(Some(VolumeMode::Hardware)); // Speakers have no control of their own
        p.set_volume_mode(Some(VolumeMode::Fixed));
        assert_eq!(p.take_wants(), vec![
            ToControl::SetOutput { output: 2 },
            ToControl::SetBalance { output: 1, balance: -0.5 },
            ToControl::SetLayout { output: 1, channels: 6 },
            ToControl::SetSpatial { output: 1, on: true },
            ToControl::SetVolumeMode { output: 1, mode: Some(VolumeMode::Fixed) },
        ]);
        assert_eq!(SoundPage::volume_modes(&p.outputs[1]).len(), 4);
    }

    #[test]
    fn nothing_is_sent_or_kept_once_choragusd_is_gone() {
        let mut p = page();
        p.lost("choragusd is not running".into());
        assert!(p.apps.is_empty() && p.outputs.is_empty());
        p.set_master(0.9);
        p.toggle_master_mute();
        p.set_balance(0.5);
        assert!(p.take_wants().is_empty());
        assert_eq!(app_name(&page().apps[0]), "Player · Video");
        assert_eq!(percent(0.426), "43%");
    }
}
