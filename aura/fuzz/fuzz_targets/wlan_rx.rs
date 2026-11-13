//! The kernel receive path: any frame bytes and receive info, against a
//! radio associated with keys installed and the port open. Must never panic.
#![no_main]

use aura_proto::{bss_flags, Bss, Cipher, Key, KeyKind, Mac, Msg, PowerSave, ScanReq, Width};
use aura_wlan_core::{reg::Regulatory, Radio, RxInfo, Wlan};
use libfuzzer_sys::fuzz_target;

const OWN: Mac = [2, 0, 0, 0, 0, 1];
const AP: Mac = [2, 0xaa, 0, 0, 0, 2];

struct R;
impl Radio for R {
    fn own_mac(&self) -> Mac { OWN }
    fn scan(&mut self, _: &ScanReq<'_>, _: &Regulatory) -> Result<(), i32> { Ok(()) }
    fn set_bss(&mut self, _: &Bss) -> Result<(), i32> { Ok(()) }
    fn clear_bss(&mut self) {}
    fn tx_mgmt(&mut self, _: u16, _: &[u8]) -> Result<(), i32> { Ok(()) }
    fn set_key(&mut self, _: &Key) -> Result<(), i32> { Ok(()) }
    fn del_key(&mut self, _: KeyKind, _: u8, _: &Mac) -> Result<(), i32> { Ok(()) }
    fn set_power_save(&mut self, _: PowerSave) -> Result<(), i32> { Ok(()) }
    fn set_mac(&mut self, _: &Mac) -> Result<(), i32> { Ok(()) }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 10 {
        return;
    }
    let mut w = Wlan::new(R);
    let bss = Bss {
        bssid: AP, freq: 2412, width: Width::W20, center1: 0, center2: 0, aid: 1,
        beacon_int: 100, dtim_period: 1, flags: bss_flags::QOS, basic_rates: 0,
        rates: 0x0ff0, ht_mcs: [0; 10], vht_rx_mcs: 0, vht_tx_mcs: 0, he_mcs: [0; 6],
        ampdu_factor: 0, ampdu_density: 0,
    };
    w.ctl(&Msg::SetBss(bss));
    if data[0] & 1 != 0 {
        let key = Key { kind: KeyKind::Pairwise, idx: 0, cipher: Cipher::Ccmp128, addr: AP, rsc: [0; 6], key: [1; 32] };
        w.keys(&Msg::SetKey(key));
    }
    if data[0] & 2 != 0 {
        w.keys(&Msg::PortAuthorized(true));
    }
    let mut pn = [0u8; 8];
    pn[..6].copy_from_slice(&data[1..7]);
    let info = RxInfo { decrypted: data[7] & 1 != 0, pn: u64::from_le_bytes(pn), key_idx: data[8] & 3 };
    if let Ok(r) = w.rx_data(&data[9..], &info) {
        for m in r.iter() {
            core::hint::black_box(m);
        }
    }
});
