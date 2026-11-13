use crate::crypto::{self, Akm, Pmk};
use crate::eapol::{self, info, KeyFrame};
use crate::rsne::{self, Rsne};
use crate::supp::{Action, Config, Ignored, Supplicant};

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn psk_vectors() {
    // IEEE 802.11-2020 J.4.2.
    let p = crypto::psk(b"password", b"IEEE").unwrap();
    assert_eq!(p.0.to_vec(), hex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"));
    let p = crypto::psk(b"ThisIsAPassword", b"ThisIsASSID").unwrap();
    assert_eq!(p.0.to_vec(), hex("0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af"));
    assert!(crypto::psk(b"short", b"x").is_none());
    assert!(crypto::psk(b"has\x01control", b"x").is_none());
    let h = "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e";
    assert_eq!(crypto::psk(h.as_bytes(), b"any").unwrap().0.to_vec(), hex(h));
}

#[test]
fn prf_sha1_vector() {
    // IEEE 802.11-2020 J.3.2, test case 1.
    let mut out = [0u8; 64];
    crypto::prf_sha1(&[0x0b; 20], b"prefix", b"Hi There", &mut out);
    assert_eq!(
        out.to_vec(),
        hex("bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee
             75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a")
    );
}

#[test]
fn key_wrap_vector() {
    // RFC 3394 §4.1.
    let kek: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
    let data = hex("00112233445566778899aabbccddeeff");
    let w = crypto::wrap(&kek, &data).unwrap();
    assert_eq!(w, hex("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5"));
    assert_eq!(crypto::unwrap(&kek, &w).unwrap(), data);
    let mut bad = w.clone();
    bad[3] ^= 1;
    assert!(crypto::unwrap(&kek, &bad).is_none());
}

#[test]
fn rsne_roundtrip() {
    let e = Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, Akm::PskSha256.suite(), rsne::CAP_MFPC, None);
    assert_eq!(e[0], 48);
    assert_eq!(e[1] as usize, e.len() - 2);
    let r = Rsne::parse(&e[2..]).unwrap();
    assert_eq!(r.group, rsne::CIPHER_CCMP128);
    assert_eq!(r.pairwise, vec![rsne::CIPHER_CCMP128]);
    assert_eq!(r.akms, vec![0x000f_ac06]);
    assert!(r.mfpc() && !r.mfpr());
    assert!(Rsne::parse(&[2, 0]).is_err());
    assert!(Rsne::parse(&[1, 0, 0, 0x0f, 0xac, 4, 0, 0]).is_err()); // zero pairwise
    assert!(Rsne::parse(&[1, 0, 0, 0x0f, 0xac, 4, 9, 0]).is_err()); // too many, short
    // Every truncation of a valid element parses or fails cleanly.
    let full = Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, 2 | 0x000f_ac00, 0xc0, Some(rsne::CIPHER_BIP_CMAC128));
    for n in 0..full.len() - 2 {
        let _ = Rsne::parse(&full[2..2 + n]);
    }
    assert_eq!(Rsne::parse(&full[2..]).unwrap().group_mgmt, Some(rsne::CIPHER_BIP_CMAC128));
}

/// A minimal authenticator to drive the supplicant (test only).
struct Ap {
    akm: Akm,
    pmk: Pmk,
    aa: [u8; 6],
    spa: [u8; 6],
    anonce: [u8; 32],
    rsne: Vec<u8>,
    replay: u64,
    ptk: Option<crypto::Ptk>,
}

const GTK: [u8; 16] = [0x47; 16];
const IGTK: [u8; 16] = [0x49; 16];

impl Ap {
    fn msg1(&mut self) -> Vec<u8> {
        self.replay += 1;
        eapol::build(2, self.akm.key_version() | info::PAIRWISE | info::ACK, 16, self.replay, &self.anonce, &[])
    }

    fn check_msg2(&mut self, f: &[u8]) -> Vec<u8> {
        let k = KeyFrame::parse(f).unwrap();
        assert_eq!(k.replay, self.replay);
        assert!(k.has(info::MIC) && k.has(info::PAIRWISE) && !k.has(info::ACK));
        let ptk = crypto::ptk(self.akm, &self.pmk, &self.aa, &self.spa, &self.anonce, k.nonce);
        assert!(crypto::mic_ok(self.akm.key_version(), &ptk.kck, &eapol::zero_mic(f), k.mic), "msg2 MIC");
        let own = k.data.to_vec();
        self.ptk = Some(ptk);
        own
    }

    fn key_data(&self, rsne: Option<&[u8]>, gtk_idx: u8, igtk: bool) -> Vec<u8> {
        let mut d = Vec::new();
        if let Some(r) = rsne {
            d.extend_from_slice(r);
        }
        d.extend_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 1, gtk_idx, 0]);
        d.extend_from_slice(&GTK);
        if igtk {
            d.extend_from_slice(&[0xdd, 28, 0x00, 0x0f, 0xac, 9, 4, 0, 1, 0, 0, 0, 0, 0]);
            d.extend_from_slice(&IGTK);
        }
        if d.len() % 8 != 0 || d.len() < 16 {
            d.push(0xdd);
            while d.len() % 8 != 0 || d.len() < 16 {
                d.push(0);
            }
        }
        crypto::wrap(&self.ptk.as_ref().unwrap().kek, &d).unwrap()
    }

    fn sign(&self, mut f: Vec<u8>) -> Vec<u8> {
        let m = crypto::mic(self.akm.key_version(), &self.ptk.as_ref().unwrap().kck, &f).unwrap();
        f[eapol::MIC_OFF..eapol::MIC_OFF + 16].copy_from_slice(&m);
        f
    }

    fn msg3(&mut self, rsne: &[u8], igtk: bool) -> Vec<u8> {
        self.replay += 1;
        let kd = self.key_data(Some(rsne), 1, igtk);
        let i = self.akm.key_version() | info::PAIRWISE | info::ACK | info::MIC | info::INSTALL | info::SECURE | info::ENCRYPTED;
        let mut f = eapol::build(2, i, 16, self.replay, &self.anonce, &kd);
        f[73] = 5; // GTK RSC
        self.sign(f)
    }

    fn group1(&mut self, idx: u8, igtk: bool) -> Vec<u8> {
        self.replay += 1;
        let kd = self.key_data(None, idx, igtk);
        let i = self.akm.key_version() | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED;
        self.sign(eapol::build(2, i, 16, self.replay, &[0; 32], &kd))
    }
}

fn setup(akm: Akm, pmf: bool) -> (Ap, Supplicant) {
    let pmk = crypto::psk(b"correct horse battery staple", b"aura-test").unwrap();
    let caps = if pmf { rsne::CAP_MFPC | rsne::CAP_MFPR } else { 0 };
    let ap_rsne = Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, akm.suite(), caps, None);
    let own_rsne = Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, akm.suite(), if pmf { rsne::CAP_MFPC } else { 0 }, None);
    let aa = [2, 0, 0, 0, 0, 0];
    let spa = [2, 0xa5, 0, 0, 0, 1];
    let ap = Ap { akm, pmk: pmk.clone(), aa, spa, anonce: [0xa1; 32], rsne: ap_rsne.clone(), replay: 0, ptk: None };
    let s = Supplicant::new(Config { akm, pmk, aa, spa, own_rsne, ap_rsne, pmf }, [0x5e; 32]);
    (ap, s)
}

fn sent(a: &[Action]) -> Vec<u8> {
    match &a[0] {
        Action::Send(b) => b.clone(),
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn four_way_both_akms() {
    for (akm, pmf) in [(Akm::Psk, false), (Akm::Psk, true), (Akm::PskSha256, true)] {
        let (mut ap, mut s) = setup(akm, pmf);
        let a = s.rx(&ap.msg1()).unwrap();
        let own = ap.check_msg2(&sent(&a));
        assert_eq!(own[0], 48);
        let rsne = ap.rsne.clone();
        let a = s.rx(&ap.msg3(&rsne, pmf)).unwrap();
        let msg4 = sent(&a);
        let k = KeyFrame::parse(&msg4).unwrap();
        assert!(k.has(info::SECURE) && k.has(info::MIC));
        let ptk = ap.ptk.as_ref().unwrap();
        assert!(crypto::mic_ok(akm.key_version(), &ptk.kck, &eapol::zero_mic(&msg4), k.mic));
        assert!(matches!(&a[1], Action::SetPtk(tk) if *tk == ptk.tk));
        assert!(matches!(&a[2], Action::SetGtk { idx: 1, key, rsc } if key[..] == GTK && rsc[0] == 5));
        if pmf {
            assert!(matches!(&a[3], Action::SetIgtk { idx: 4, key, ipn } if key[..] == IGTK && ipn[0] == 1));
        }
        assert!(matches!(a.last(), Some(Action::Authorized)));
        assert!(s.authorized());

        // Retransmitted msg 3 (same counter): replay. With a new counter:
        // answered, nothing reinstalled (KRACK).
        let again = ap.msg3(&rsne, pmf);
        let a = s.rx(&again).unwrap();
        assert_eq!(a.len(), 1);
        assert!(s.rx(&again).is_err_and(|e| e == Ignored::Replay));

        // Group rekey.
        let a = s.rx(&ap.group1(2, pmf)).unwrap();
        assert!(matches!(&a[1], Action::SetGtk { idx: 2, .. }));
        let g2b = sent(&a);
        let g2 = KeyFrame::parse(&g2b).unwrap();
        assert!(!g2.has(info::PAIRWISE) && g2.has(info::SECURE));
    }
}

#[test]
fn msg3_rules() {
    let (mut ap, mut s) = setup(Akm::PskSha256, true);
    let a = s.rx(&ap.msg1()).unwrap();
    ap.check_msg2(&sent(&a));
    // Downgraded RSNE (no MFPR): refused.
    let weaker = Rsne::build(rsne::CIPHER_CCMP128, rsne::CIPHER_CCMP128, Akm::PskSha256.suite(), rsne::CAP_MFPC, None);
    assert_eq!(s.rx(&ap.msg3(&weaker, true)).unwrap_err(), Ignored::RsneMismatch);
    // PMF without IGTK: refused.
    let rsne = ap.rsne.clone();
    assert_eq!(s.rx(&ap.msg3(&rsne, false)).unwrap_err(), Ignored::NoIgtk);
    // Bad MIC: refused before anything else.
    let mut m = ap.msg3(&rsne, true);
    m[eapol::MIC_OFF] ^= 1;
    assert_eq!(s.rx(&m).unwrap_err(), Ignored::BadMic);
    // Wrong ANonce.
    ap.anonce[0] ^= 1;
    assert_eq!(s.rx(&ap.msg3(&rsne, true)).unwrap_err(), Ignored::AnonceChanged);
    ap.anonce[0] ^= 1;
    assert!(!s.authorized());
    assert!(s.rx(&ap.msg3(&rsne, true)).is_ok());
    // Group message before the 4-way: refused (fresh supplicant).
    let (mut ap2, mut s2) = setup(Akm::Psk, false);
    let _ = s2.rx(&ap2.msg1()).unwrap();
    ap2.ptk = Some(crypto::ptk(Akm::Psk, &ap2.pmk, &ap2.aa, &ap2.spa, &ap2.anonce, &[0x5e; 32]));
    assert_eq!(s2.rx(&ap2.group1(1, false)).unwrap_err(), Ignored::UnexpectedMessage);
}

#[test]
fn parsers_total() {
    // Mutations of valid frames never panic.
    let (mut ap, mut s) = setup(Akm::Psk, true);
    let m1 = ap.msg1();
    let a = s.rx(&m1).unwrap();
    ap.check_msg2(&sent(&a));
    let rsne = ap.rsne.clone();
    let m3 = ap.msg3(&rsne, true);
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for i in 0..50_000 {
        let base = if i % 2 == 0 { &m1 } else { &m3 };
        let mut f = base.clone();
        for _ in 0..1 + (i % 4) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let p = (x as usize) % f.len();
            f[p] = (x >> 32) as u8;
        }
        if i % 7 == 0 {
            f.truncate((x as usize) % (f.len() + 1));
        }
        let _ = s.rx(&f);
        let _ = eapol::parse_key_data(&f);
        let _ = Rsne::parse(&f);
    }
}
