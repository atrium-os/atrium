//! Routing, ducking, per-app volume, hotplug-follow, exclusive-claim (§7).
//!
//! [`Session::resolve`] is **pure** — given the streams, devices, and rules it
//! computes the desired `(sink, gain)` per stream with no scheduling, mixing, or
//! audio-path side effect. [`diff`] turns a desired-state change into the
//! mechanism-agnostic [`Change`]s lyrad applies: a `GainRamp` (the zipper-free
//! smoother) or a `Reroute` (the §12.2 glitch-free atomic-commit reconfiguration).

/// An app's declared audio role (from its `atrium.toml`). Informs routing + duck.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Media,
    Communication,
    Notification,
    Game,
    Pro,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    Speakers,
    Headset,
    Hdmi,
}

pub type DeviceId = u32;
pub type StreamId = u32;

#[derive(Clone, Copy)]
pub struct Device {
    pub id: DeviceId,
    pub kind: DeviceKind,
}

#[derive(Clone)]
pub struct Stream {
    pub id: StreamId,
    pub role: Role,
    /// Explicit user routing — overrides the role default when set.
    pub user_sink: Option<DeviceId>,
    /// Explicit user volume (dB) — the base the policy adjusts from.
    pub user_gain_db: Option<f32>,
    /// Requires sole ownership of its device (bit-perfect / passthrough / DSD,
    /// §4.3): a second stream cannot share the device while this is open.
    pub exclusive: bool,
}

impl Stream {
    pub fn new(id: StreamId, role: Role) -> Self {
        Stream { id, role, user_sink: None, user_gain_db: None, exclusive: false }
    }
}

/// The declarative policy — data the user/manifests own, not engine code.
#[derive(Clone, Copy)]
pub struct Policy {
    /// How far to duck Media/Game when a Communication stream is active (dB, ≤0).
    pub duck_db: f32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { duck_db: -18.0 }
    }
}

/// The resolved desired state for one stream — what the engine should realise.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Decision {
    pub stream: StreamId,
    pub sink: DeviceId,
    pub gain_db: f32,
}

#[derive(Debug, PartialEq)]
pub enum PolicyError {
    /// The device is exclusively held (or this exclusive open conflicts).
    DeviceBusy { device: DeviceId },
    /// No output device exists to route to.
    NoDevice,
}

pub struct Session {
    pub devices: Vec<Device>,
    pub streams: Vec<Stream>,
    pub policy: Policy,
    /// The current system default output.
    pub default_device: DeviceId,
}

impl Session {
    pub fn new(devices: Vec<Device>, default_device: DeviceId) -> Self {
        Session { devices, streams: Vec::new(), policy: Policy::default(), default_device }
    }

    fn first_of(&self, kind: DeviceKind) -> Option<DeviceId> {
        self.devices.iter().find(|d| d.kind == kind).map(|d| d.id)
    }

    /// The default sink a role prefers, before user override. Communication
    /// prefers a headset when present (keep the call off the speakers);
    /// everything else follows the system default.
    fn role_default_sink(&self, role: Role) -> DeviceId {
        match role {
            Role::Communication => {
                self.first_of(DeviceKind::Headset).unwrap_or(self.default_device)
            }
            _ => self.default_device,
        }
    }

    fn sink_for(&self, s: &Stream) -> DeviceId {
        s.user_sink.unwrap_or_else(|| self.role_default_sink(s.role))
    }

    /// Admit a stream, enforcing exclusivity (§4.3). Fails if it would share a
    /// device with — or is itself exclusive against — an existing stream.
    pub fn open(&mut self, s: Stream) -> Result<(), PolicyError> {
        if self.devices.is_empty() {
            return Err(PolicyError::NoDevice);
        }
        let target = self.sink_for(&s);
        for other in &self.streams {
            if self.sink_for(other) == target && (other.exclusive || s.exclusive) {
                return Err(PolicyError::DeviceBusy { device: target });
            }
        }
        self.streams.push(s);
        Ok(())
    }

    pub fn close(&mut self, id: StreamId) {
        self.streams.retain(|s| s.id != id);
    }

    /// Device hotplug: a new device appears (a headset becomes the default).
    pub fn plug(&mut self, dev: Device) {
        let becomes_default = dev.kind == DeviceKind::Headset;
        self.devices.push(dev);
        if becomes_default {
            self.default_device = dev.id;
        }
    }

    /// Device unplug: remove it; anything defaulting to it falls back.
    pub fn unplug(&mut self, id: DeviceId) {
        self.devices.retain(|d| d.id != id);
        if self.default_device == id {
            self.default_device = self.devices.first().map(|d| d.id).unwrap_or(0);
        }
    }

    /// PURE: compute the desired `(sink, gain)` for every stream. No scheduling,
    /// no mixing, no audio-path side effect — just policy over the current state.
    pub fn resolve(&self) -> Vec<Decision> {
        let comms_active = self.streams.iter().any(|s| s.role == Role::Communication);
        self.streams
            .iter()
            .map(|s| {
                let base = s.user_gain_db.unwrap_or(0.0);
                // Communication ducks Media and Game (not itself, not Pro).
                let duck = if comms_active && matches!(s.role, Role::Media | Role::Game) {
                    self.policy.duck_db
                } else {
                    0.0
                };
                Decision { stream: s.id, sink: self.sink_for(s), gain_db: base + duck }
            })
            .collect()
    }
}

/// A mechanism-agnostic change the engine applies — the only thing this layer
/// hands the RT engine. A gain change is a zipper-free ramp; a sink change is a
/// glitch-free atomic-commit reconfiguration (§12.2).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Change {
    GainRamp { stream: StreamId, to_db: f32 },
    Reroute { stream: StreamId, to_sink: DeviceId },
}

/// Diff two desired states into the changes that realise the transition. Policy
/// emits these; it never recomputes a schedule itself.
pub fn diff(old: &[Decision], new: &[Decision]) -> Vec<Change> {
    let mut out = Vec::new();
    for n in new {
        if let Some(o) = old.iter().find(|o| o.stream == n.stream) {
            if (o.gain_db - n.gain_db).abs() > 1e-6 {
                out.push(Change::GainRamp { stream: n.stream, to_db: n.gain_db });
            }
            if o.sink != n.sink {
                out.push(Change::Reroute { stream: n.stream, to_sink: n.sink });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPK: DeviceId = 0;
    const HS: DeviceId = 1;

    fn speakers_only() -> Session {
        Session::new(vec![Device { id: SPK, kind: DeviceKind::Speakers }], SPK)
    }
    fn gain_of(d: &[Decision], s: StreamId) -> f32 {
        d.iter().find(|x| x.stream == s).unwrap().gain_db
    }
    fn sink_of(d: &[Decision], s: StreamId) -> DeviceId {
        d.iter().find(|x| x.stream == s).unwrap().sink
    }

    #[test]
    fn communication_ducks_media_and_restores() {
        let mut s = speakers_only();
        s.open(Stream::new(0, Role::Media)).unwrap();
        assert_eq!(gain_of(&s.resolve(), 0), 0.0);
        s.open(Stream::new(1, Role::Communication)).unwrap();
        assert_eq!(gain_of(&s.resolve(), 0), -18.0, "media ducked under the call");
        assert_eq!(gain_of(&s.resolve(), 1), 0.0, "the call is not ducked");
        s.close(1);
        assert_eq!(gain_of(&s.resolve(), 0), 0.0, "restored");
    }

    #[test]
    fn pro_is_never_ducked() {
        let mut s = speakers_only();
        s.open(Stream::new(0, Role::Pro)).unwrap();
        s.open(Stream::new(1, Role::Communication)).unwrap();
        assert_eq!(gain_of(&s.resolve(), 0), 0.0);
    }

    #[test]
    fn user_volume_is_the_base_the_duck_stacks_on() {
        let mut s = speakers_only();
        let mut m = Stream::new(0, Role::Media);
        m.user_gain_db = Some(-6.0);
        s.open(m).unwrap();
        assert_eq!(gain_of(&s.resolve(), 0), -6.0);
        s.open(Stream::new(1, Role::Communication)).unwrap();
        assert_eq!(gain_of(&s.resolve(), 0), -24.0);
    }

    #[test]
    fn comms_prefers_headset_user_route_overrides() {
        let mut s = Session::new(
            vec![
                Device { id: SPK, kind: DeviceKind::Speakers },
                Device { id: HS, kind: DeviceKind::Headset },
            ],
            SPK,
        );
        s.open(Stream::new(0, Role::Media)).unwrap();
        s.open(Stream::new(1, Role::Communication)).unwrap();
        let d = s.resolve();
        assert_eq!(sink_of(&d, 0), SPK);
        assert_eq!(sink_of(&d, 1), HS, "comms prefers the headset by role");
        s.streams[1].user_sink = Some(SPK);
        assert_eq!(sink_of(&s.resolve(), 1), SPK, "user route overrides the role");
    }

    #[test]
    fn hotplug_headset_becomes_default_and_streams_follow() {
        let mut s = speakers_only();
        s.open(Stream::new(0, Role::Media)).unwrap();
        let before = s.resolve();
        s.plug(Device { id: HS, kind: DeviceKind::Headset });
        let after = s.resolve();
        assert_eq!(sink_of(&after, 0), HS);
        assert_eq!(diff(&before, &after), vec![Change::Reroute { stream: 0, to_sink: HS }]);
        s.unplug(HS);
        assert_eq!(sink_of(&s.resolve(), 0), SPK, "falls back on unplug");
    }

    #[test]
    fn exclusive_claim_refuses_a_second_stream() {
        let mut s = speakers_only();
        let mut excl = Stream::new(0, Role::Media);
        excl.exclusive = true;
        s.open(excl).unwrap();
        assert_eq!(
            s.open(Stream::new(1, Role::Media)),
            Err(PolicyError::DeviceBusy { device: SPK })
        );
        s.close(0);
        assert!(s.open(Stream::new(2, Role::Media)).is_ok());
    }

    #[test]
    fn policy_emits_only_mechanism_agnostic_changes() {
        let mut s = speakers_only();
        s.open(Stream::new(0, Role::Media)).unwrap();
        let before = s.resolve();
        s.open(Stream::new(1, Role::Communication)).unwrap();
        let changes = diff(&before, &s.resolve());
        assert!(changes.contains(&Change::GainRamp { stream: 0, to_db: -18.0 }));
        assert!(changes
            .iter()
            .all(|c| matches!(c, Change::GainRamp { .. } | Change::Reroute { .. })));
    }
}
