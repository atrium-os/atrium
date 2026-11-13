//! The key table: what is installed in hardware, the KRACK rule, and the
//! per-TID replay counters (802.11-2020 §12.5.3.4.4).

use aura_proto::{Cipher, Key, KeyKind, Mac};

/// Replay counter slots: TIDs 0-7, then one for management frames.
pub const RX_SLOTS: usize = 9;
pub const MGMT_SLOT: usize = 8;
const PN_MAX: u64 = (1 << 48) - 1;

pub struct Slot {
    pub cipher: Cipher,
    material: [u8; 32],
    pub addr: Mac,
    /// Highest PN accepted per slot; `None` before the first frame, in which
    /// case the PN must exceed `start`.
    last: [Option<u64>; RX_SLOTS],
    start: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.material = [0; 32];
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// New key: program the hardware.
    New,
    /// Identical to the installed key: do nothing, and above all do not
    /// reset its replay counters (KRACK, CVE-2017-13077..13088).
    Unchanged,
}

#[derive(Default)]
pub struct Table {
    pairwise: [Option<Slot>; 2],
    group: [Option<Slot>; 3],
    igtk: [Option<Slot>; 2],
    bigtk: [Option<Slot>; 2],
}

fn rsc_u64(r: &[u8; 6]) -> u64 {
    let mut b = [0u8; 8];
    b[..6].copy_from_slice(r);
    u64::from_le_bytes(b)
}

/// Constant-time comparison of key material.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Table {
    fn slot_mut(&mut self, kind: KeyKind, idx: u8) -> Option<&mut Option<Slot>> {
        let i = idx as usize;
        match kind {
            KeyKind::Pairwise => self.pairwise.get_mut(i),
            KeyKind::Group => self.group.get_mut(i.checked_sub(1)?),
            KeyKind::Igtk => self.igtk.get_mut(i.checked_sub(4)?),
            KeyKind::Bigtk => self.bigtk.get_mut(i.checked_sub(6)?),
        }
    }

    pub fn get(&self, kind: KeyKind, idx: u8) -> Option<&Slot> {
        let i = idx as usize;
        match kind {
            KeyKind::Pairwise => self.pairwise.get(i)?.as_ref(),
            KeyKind::Group => self.group.get(i.checked_sub(1)?)?.as_ref(),
            KeyKind::Igtk => self.igtk.get(i.checked_sub(4)?)?.as_ref(),
            KeyKind::Bigtk => self.bigtk.get(i.checked_sub(6)?)?.as_ref(),
        }
    }

    pub fn install(&mut self, k: &Key) -> Install {
        let Some(slot) = self.slot_mut(k.kind, k.idx) else {
            // aura-proto validated the index range.
            return Install::Unchanged;
        };
        if let Some(s) = slot {
            if s.cipher == k.cipher && s.addr == k.addr && same(&s.material, &k.key) {
                return Install::Unchanged;
            }
        }
        *slot = Some(Slot {
            cipher: k.cipher,
            material: k.key,
            addr: k.addr,
            last: [None; RX_SLOTS],
            start: rsc_u64(&k.rsc),
        });
        Install::New
    }

    pub fn remove(&mut self, kind: KeyKind, idx: u8) -> bool {
        match self.slot_mut(kind, idx) {
            Some(s) => s.take().is_some(),
            None => false,
        }
    }

    pub fn clear(&mut self) {
        *self = Table::default();
    }

    pub fn has_pairwise(&self) -> bool {
        self.pairwise.iter().any(|s| s.is_some())
    }

    pub fn has_group(&self) -> bool {
        self.group.iter().any(|s| s.is_some())
    }

    /// Accepts `pn` for (`kind`, `idx`, `slot`) only if it is larger than
    /// every PN accepted before (or than the installed RSC), and records it.
    /// Returns false for a replay, an unknown key or a PN past 2^48-1.
    pub fn check_pn(&mut self, kind: KeyKind, idx: u8, slot: usize, pn: u64) -> bool {
        let Some(Some(s)) = self.slot_mut(kind, idx) else {
            return false;
        };
        if slot >= RX_SLOTS || pn > PN_MAX {
            return false;
        }
        let ok = match s.last[slot] {
            Some(last) => pn > last,
            None => pn > s.start || (s.start == 0 && pn >= 1),
        };
        if ok {
            s.last[slot] = Some(pn);
        }
        ok
    }
}
