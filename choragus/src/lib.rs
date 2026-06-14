//! Choragus — Atrium's audio policy / session layer (the "audio window manager").
//! Design: `docs/spec/atrium-lyra-architecture.md` §7 + §9; named in
//! `docs/NAMING.md` (the citizen who arranged the chorus — *Lyra plays; the
//! Choragus arranges who plays*).
//!
//! This is the **production** counterpart of the gpusim `choragus` model: pure,
//! non-RT policy. [`policy`] decides which app → which sink, ducking, per-app
//! volume, hotplug-follow, and exclusive-claim; [`capability`] enforces the
//! §9 privacy capabilities (default-deny; the global mix is never visible
//! without `audio_monitor`). The RT engine (lyrad) is mechanism only — it
//! consumes the mechanism-agnostic [`policy::Change`]s this layer emits (a gain
//! ramp, a re-route applied by the glitch-free reconfiguration), nothing more.

pub mod app;
pub mod capability;
pub mod control;
pub mod grant;
pub mod policy;
