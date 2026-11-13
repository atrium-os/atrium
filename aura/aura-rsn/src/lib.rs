//! RSN for Aura's `aura-keys` (docs/spec/wifi.md §3.4, §8.3): PSK and PTK
//! derivation, the RSN element, EAPOL-Key frames and the supplicant side of
//! the 4-way and group key handshakes. No I/O, no unsafe; key material is
//! zeroed on drop.
//!
//! References: IEEE 802.11-2020 §12.7 and Annex J test vectors; the shape
//! follows OpenBSD's ieee80211_crypto/pae code and Fuchsia's wlan-rsn
//! (both permissive, spec §4), written fresh.

#![forbid(unsafe_code)]

pub mod crypto;
pub mod eapol;
pub mod rsne;
pub mod supp;

#[cfg(test)]
mod tests;
