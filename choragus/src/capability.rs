//! Privacy & capability enforcement (§9).
//!
//! Audio access is capability-gated and **default-deny**. Portcullis *grants* the
//! capability (from the manifest + the user); Choragus *enforces* what audio
//! access each grant permits. The load-bearing property: the **global mix is
//! never visible without [`Capability::AudioMonitor`]** — designed against
//! PulseAudio's monitor-source leak. Capture ([`Capability::Microphone`]) and
//! monitor ([`Capability::AudioMonitor`]) are **distinct**: a conferencing app
//! needs the mic, not the right to record everything you hear.

use crate::policy::StreamId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Capability {
    /// Play to *your own* routed sink; observe only your own streams.
    Audio,
    /// Capture from a microphone device. User-visible.
    Microphone,
    /// Tap the system output or another stream (loopback) — the audio
    /// screen-record. Prominently surfaced; never implied by `Audio`.
    AudioMonitor,
}

/// The capability set Portcullis granted an app.
#[derive(Default, Clone)]
pub struct Grant {
    caps: Vec<Capability>,
}

impl Grant {
    pub fn of(caps: &[Capability]) -> Self {
        Grant { caps: caps.to_vec() }
    }
    pub fn has(&self, c: Capability) -> bool {
        self.caps.contains(&c)
    }
}

/// An audio access an app may attempt. Each maps to the one capability it needs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Access {
    /// Play to your own routed sink.
    Play,
    /// Observe your own streams (levels, state).
    SeeOwnStreams,
    /// Capture from a microphone.
    CaptureMic,
    /// Tap the global system mix (the monitor / loopback).
    TapSystemMix,
    /// Tap another app's stream.
    TapStream(StreamId),
}

#[derive(Debug, PartialEq)]
pub struct Denied {
    pub access: Access,
    pub needs: Capability,
}

/// The capability `access` requires.
pub fn required(access: Access) -> Capability {
    match access {
        Access::Play | Access::SeeOwnStreams => Capability::Audio,
        Access::CaptureMic => Capability::Microphone,
        // the ONLY paths to anything beyond your own streams need AudioMonitor.
        Access::TapSystemMix | Access::TapStream(_) => Capability::AudioMonitor,
    }
}

/// Enforce §9: allow `access` only if the grant holds the required capability.
/// Default-deny — an absent capability is a refusal, never a silent pass.
pub fn check(grant: &Grant, access: Access) -> Result<(), Denied> {
    let needs = required(access);
    if grant.has(needs) {
        Ok(())
    } else {
        Err(Denied { access, needs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_plays_and_sees_own_but_not_mix_or_mic() {
        let g = Grant::of(&[Capability::Audio]);
        assert!(check(&g, Access::Play).is_ok());
        assert!(check(&g, Access::SeeOwnStreams).is_ok());
        assert_eq!(
            check(&g, Access::TapSystemMix),
            Err(Denied { access: Access::TapSystemMix, needs: Capability::AudioMonitor }),
            "the global mix is never visible to a plain audio app"
        );
        assert!(check(&g, Access::CaptureMic).is_err(), "audio is not the mic");
    }

    #[test]
    fn microphone_and_monitor_are_distinct() {
        let conf = Grant::of(&[Capability::Audio, Capability::Microphone]);
        assert!(check(&conf, Access::CaptureMic).is_ok());
        assert!(check(&conf, Access::TapSystemMix).is_err(), "mic ≠ system tap");
        let mon = Grant::of(&[Capability::AudioMonitor]);
        assert!(check(&mon, Access::TapSystemMix).is_ok());
        assert!(check(&mon, Access::CaptureMic).is_err(), "monitor ≠ mic");
    }

    #[test]
    fn audio_monitor_is_the_only_path_to_other_streams() {
        assert_eq!(required(Access::TapSystemMix), Capability::AudioMonitor);
        assert_eq!(required(Access::TapStream(7)), Capability::AudioMonitor);
        let mon = Grant::of(&[Capability::AudioMonitor]);
        assert!(check(&mon, Access::TapStream(7)).is_ok());
    }

    #[test]
    fn default_deny_for_the_empty_grant() {
        let none = Grant::default();
        for a in [
            Access::Play,
            Access::SeeOwnStreams,
            Access::CaptureMic,
            Access::TapSystemMix,
            Access::TapStream(0),
        ] {
            assert!(check(&none, a).is_err(), "no capability → {a:?} denied");
        }
    }
}
