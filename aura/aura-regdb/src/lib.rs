//! Aura's regulatory database (docs/spec/wifi.md §3.6, decision 9).
//!
//! wireless-regdb's `db.txt` (ISC licence, `data/`) is compiled into
//! [`COUNTRIES`] at build time, so the table is part of the signed system
//! image rather than a file read at run time. aura-mlme picks a country's
//! rules and sends them to the kernel (`SetRegulatory`), which enforces
//! them; an AP's Country element can only narrow them ([`intersect`]).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
pub use aura_proto::RegRule;
use aura_proto::{reg_flags, MAX_REG_RULES};

pub struct Country {
    pub alpha2: [u8; 2],
    pub rules: &'static [RegRule],
}

include!(concat!(env!("OUT_DIR"), "/db.rs"));

/// The world rules ("00"), used until a country is known.
pub fn world() -> &'static Country {
    country(*b"00").expect("regdb has 00")
}

/// A country's rules, by ISO 3166 alpha-2 code (upper case).
pub fn country(alpha2: [u8; 2]) -> Option<&'static Country> {
    COUNTRIES.iter().find(|c| c.alpha2 == alpha2)
}

/// The rules allowed by both `a` and `b`: overlapping ranges only, the
/// smaller bandwidth and power, the union of the restrictions. The result
/// never allows anything `a` does not (tighten only).
pub fn intersect(a: &[RegRule], b: &[RegRule]) -> Vec<RegRule> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let start = x.start_khz.max(y.start_khz);
            let end = x.end_khz.min(y.end_khz);
            if end <= start || out.len() == MAX_REG_RULES {
                continue;
            }
            out.push(RegRule {
                start_khz: start,
                end_khz: end,
                max_bw_khz: x.max_bw_khz.min(y.max_bw_khz).min(end - start),
                max_eirp_mbm: x.max_eirp_mbm.min(y.max_eirp_mbm),
                flags: (x.flags | y.flags) & reg_flags::ALL,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table() {
        assert!(COUNTRIES.len() > 150);
        let w = world();
        // 2.4 GHz channels 1-11 usable everywhere; 5 GHz passive.
        let r = w.rules.iter().find(|r| r.start_khz == 2_402_000).unwrap();
        assert_eq!(r.flags, 0);
        assert!(w.rules.iter().filter(|r| r.start_khz >= 5_000_000).all(|r| r.flags & reg_flags::NO_IR != 0));
        let de = country(*b"DE").unwrap();
        assert!(de.rules.len() <= MAX_REG_RULES);
        // 100 mW = 20 dBm.
        assert!(de.rules.iter().any(|r| r.start_khz == 2_400_000 && r.max_eirp_mbm == 2000));
        assert!(de.rules.iter().any(|r| r.flags & reg_flags::DFS != 0));
        for c in COUNTRIES {
            assert!(c.rules.len() <= MAX_REG_RULES, "{:?}", c.alpha2);
            assert!(c.rules.iter().all(|r| r.start_khz < r.end_khz
                && r.max_bw_khz > 0
                && r.max_bw_khz <= r.end_khz - r.start_khz
                && (0..=3600).contains(&r.max_eirp_mbm)));
        }
        assert!(country(*b"ZZ").is_none());
    }

    #[test]
    fn intersection_only_tightens() {
        let us = country(*b"US").unwrap().rules;
        let jp = country(*b"JP").unwrap().rules;
        let w = world().rules;
        for (a, b) in [(us, jp), (w, us), (jp, w)] {
            for r in intersect(a, b) {
                // Every resulting rule lies inside some rule of `a` with no
                // more power or bandwidth and at least its restrictions.
                assert!(a.iter().any(|x| x.start_khz <= r.start_khz
                    && r.end_khz <= x.end_khz
                    && r.max_eirp_mbm <= x.max_eirp_mbm
                    && r.max_bw_khz <= x.max_bw_khz
                    && r.flags & x.flags == x.flags));
            }
        }
        // Channel 13 (2472 MHz): allowed in JP, not in the US.
        let ok = |rules: &[RegRule], mhz: u32| rules.iter().any(|r| r.start_khz <= mhz * 1000 - 10_000 && mhz * 1000 + 10_000 <= r.end_khz);
        assert!(ok(jp, 2472) && !ok(us, 2472));
        assert!(!ok(&intersect(jp, us), 2472));
    }
}
