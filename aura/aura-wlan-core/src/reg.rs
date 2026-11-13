//! Regulatory enforcement: the kernel keeps the final rule list `aura-mlme`
//! set (wireless-regdb, wifi.md §3.6) and refuses anything outside it.

use aura_proto::{reg_flags, RegRule, MAX_REG_RULES};

pub struct Regulatory {
    pub alpha2: [u8; 2],
    rules: [Option<RegRule>; MAX_REG_RULES],
}

impl Default for Regulatory {
    /// Before `aura-mlme` sets a country: the "00" world rules, which allow
    /// only passive operation everywhere except 2.4 GHz channels 1-11.
    fn default() -> Self {
        let mut r = Regulatory { alpha2: *b"00", rules: [None; MAX_REG_RULES] };
        r.rules[0] = Some(RegRule {
            start_khz: 2_402_000,
            end_khz: 2_472_000,
            max_bw_khz: 40_000,
            max_eirp_mbm: 2000,
            flags: 0,
        });
        r.rules[1] = Some(RegRule {
            start_khz: 2_457_000,
            end_khz: 2_482_000,
            max_bw_khz: 20_000,
            max_eirp_mbm: 2000,
            flags: reg_flags::NO_IR,
        });
        r.rules[2] = Some(RegRule {
            start_khz: 5_170_000,
            end_khz: 5_835_000,
            max_bw_khz: 160_000,
            max_eirp_mbm: 2000,
            flags: reg_flags::NO_IR,
        });
        r.rules[3] = Some(RegRule {
            start_khz: 5_925_000,
            end_khz: 7_125_000,
            max_bw_khz: 160_000,
            max_eirp_mbm: 1400,
            flags: reg_flags::NO_IR,
        });
        r
    }
}

impl Regulatory {
    pub fn set(&mut self, alpha2: [u8; 2], rules: impl Iterator<Item = RegRule>) {
        self.alpha2 = alpha2;
        self.rules = [None; MAX_REG_RULES];
        for (slot, r) in self.rules.iter_mut().zip(rules) {
            *slot = Some(r);
        }
    }

    /// The rule covering the 20 MHz channel centred on `mhz`, if any. Where
    /// two rules overlap, the stricter flags win.
    fn flags_for(&self, mhz: u16) -> Option<u32> {
        let lo = mhz as u32 * 1000 - 10_000;
        let hi = mhz as u32 * 1000 + 10_000;
        let mut out: Option<u32> = None;
        for r in self.rules.iter().flatten() {
            if r.start_khz <= lo && hi <= r.end_khz {
                out = Some(out.map_or(r.flags, |f| f | r.flags));
            }
        }
        out
    }

    /// The channel may be used at all (receive, associate to an AP there).
    pub fn allowed(&self, mhz: u16) -> bool {
        self.flags_for(mhz).is_some()
    }

    /// We may transmit first on this channel (probe requests, authentication
    /// before an AP was heard there).
    pub fn may_initiate(&self, mhz: u16) -> bool {
        matches!(self.flags_for(mhz), Some(f) if f & reg_flags::NO_IR == 0)
    }
}
