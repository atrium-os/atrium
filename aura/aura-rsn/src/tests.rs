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
    rsnxe: Option<Vec<u8>>,
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
            if let Some(x) = &self.rsnxe {
                d.extend_from_slice(x);
            }
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
    // SAE with H2E: both sides send an RSNXE (SAE H2E bit).
    let rsnxe = (akm == Akm::Sae).then(|| vec![244, 1, 0x20]);
    let ap = Ap { akm, pmk: pmk.clone(), aa, spa, anonce: [0xa1; 32], rsne: ap_rsne.clone(), rsnxe: rsnxe.clone(), replay: 0, ptk: None };
    let cfg = Config { akm, pmk, aa, spa, own_rsne, ap_rsne, own_rsnxe: rsnxe.clone(), ap_rsnxe: rsnxe, pmf };
    let s = Supplicant::new(cfg, [0x5e; 32]);
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
    for (akm, pmf) in [(Akm::Psk, false), (Akm::Psk, true), (Akm::PskSha256, true), (Akm::Sae, true)] {
        let (mut ap, mut s) = setup(akm, pmf);
        let a = s.rx(&ap.msg1()).unwrap();
        let own = ap.check_msg2(&sent(&a));
        assert_eq!(own[0], 48);
        if akm == Akm::Sae {
            // msg 2 carries our RSNE then our RSNXE.
            assert_eq!(&own[own[1] as usize + 2..], &[244, 1, 0x20]);
        }
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
fn sae_rsnxe_and_transition_disable() {
    // msg 3 without the AP's RSNXE (an H2E downgrade): refused.
    let (mut ap, mut s) = setup(Akm::Sae, true);
    let a = s.rx(&ap.msg1()).unwrap();
    ap.check_msg2(&sent(&a));
    let rsne = ap.rsne.clone();
    let x = ap.rsnxe.take();
    assert_eq!(s.rx(&ap.msg3(&rsne, true)).unwrap_err(), Ignored::RsnxeMismatch);
    ap.rsnxe = x;
    assert!(s.rx(&ap.msg3(&rsne, true)).is_ok());
    // Transition Disable KDE comes out as an action.
    let mut kd = vec![0xdd, 5, 0x50, 0x6f, 0x9a, 0x20, 1];
    kd.extend_from_slice(&[0xdd, 0, 0, 0, 0, 0, 0, 0, 0]);
    let k = eapol::parse_key_data(&kd).unwrap();
    assert_eq!(k.transition_disable, Some(1));
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

mod sae_tests {
    use crate::sae::{self, Sae, SaeError};

    const A: [u8; 6] = [0x4d, 0x3f, 0x2f, 0xff, 0xe3, 0x87];
    const B: [u8; 6] = [0xa5, 0xd8, 0xaa, 0x95, 0x8e, 0x3c];

    fn rng(seed: u8) -> impl FnMut(&mut [u8]) {
        let mut n = seed as u64;
        move |b: &mut [u8]| {
            for x in b.iter_mut() {
                n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *x = (n >> 33) as u8;
            }
        }
    }

    fn run(h2e: bool, pw_a: &[u8], pw_b: &[u8]) -> Result<([u8; 32], [u8; 32]), SaeError> {
        let (pa, pb) = if h2e {
            (sae::pwe_from_pt(&sae::pt_h2e(pw_a, b"byteme"), &A, &B), sae::pwe_from_pt(&sae::pt_h2e(pw_b, b"byteme"), &B, &A))
        } else {
            (sae::pwe_hnp(pw_a, &A, &B).unwrap(), sae::pwe_hnp(pw_b, &B, &A).unwrap())
        };
        let mut a = Sae::new(pa, h2e, &mut rng(1));
        let mut b = Sae::new(pb, h2e, &mut rng(2));
        let (ca, cb) = (a.commit_body(None), b.commit_body(None));
        a.rx_commit(&cb)?;
        b.rx_commit(&ca)?;
        let (fa, fb) = (a.confirm_body()?, b.confirm_body()?);
        b.rx_confirm(&fa)?;
        a.rx_confirm(&fb)?;
        let (ka, ia) = a.pmk().unwrap();
        let (kb, ib) = b.pmk().unwrap();
        assert_eq!(ia, ib);
        Ok((ka, kb))
    }

    #[test]
    fn both_sides_agree() {
        for h2e in [false, true] {
            let (a, b) = run(h2e, b"mekmitasdigoat", b"mekmitasdigoat").unwrap();
            assert_eq!(a, b, "h2e {h2e}");
        }
    }

    #[test]
    fn wrong_password_fails_confirm() {
        for h2e in [false, true] {
            assert_eq!(run(h2e, b"mekmitasdigoat", b"mekmitasdigoaT").unwrap_err(), SaeError::BadConfirm);
        }
    }

    #[test]
    fn pwe_symmetric_in_addresses() {
        assert_eq!(sae::pwe_hnp(b"pw", &A, &B).unwrap(), sae::pwe_hnp(b"pw", &B, &A).unwrap());
        let pt = sae::pt_h2e(b"pw", b"ssid");
        assert_eq!(sae::pwe_from_pt(&pt, &A, &B), sae::pwe_from_pt(&pt, &B, &A));
        assert_ne!(sae::pwe_hnp(b"pw", &A, &B).unwrap(), sae::pwe_hnp(b"pw2", &A, &B).unwrap());
    }

    #[test]
    fn hostile_commits() {
        let pwe = sae::pwe_hnp(b"pw", &A, &B).unwrap();
        let mut a = Sae::new(pwe, false, &mut rng(3));
        let own = a.commit_body(None);
        // Reflection.
        assert_eq!(a.rx_commit(&own).unwrap_err(), SaeError::Reflection);
        // Wrong group, short, scalar 0/1/r, element off the curve.
        let mut g = own.clone();
        g[0] = 20;
        assert_eq!(a.rx_commit(&g).unwrap_err(), SaeError::Group);
        assert_eq!(a.rx_commit(&own[..50]).unwrap_err(), SaeError::Malformed);
        for s in [[0u8; 32], { let mut x = [0u8; 32]; x[31] = 1; x }, [0xff; 32]] {
            let mut c = own.clone();
            c[2..34].copy_from_slice(&s);
            assert_eq!(a.rx_commit(&c).unwrap_err(), SaeError::BadScalar);
        }
        let mut e = own.clone();
        e[2 + 32 + 63] ^= 1;
        assert_eq!(a.rx_commit(&e).unwrap_err(), SaeError::BadElement);
        // Confirm before commit.
        assert_eq!(a.rx_confirm(&[0u8; 34]).unwrap_err(), SaeError::UnexpectedMessage);
        // Mutation fuzz: nothing panics.
        let mut r = rng(9);
        for _ in 0..300 {
            let mut c = own.clone();
            let mut x = [0u8; 3];
            r(&mut x);
            let i = x[0] as usize % c.len();
            c[i] ^= x[1] | 1;
            c.truncate(c.len() - (x[2] as usize % 4));
            let _ = a.rx_commit(&c);
        }
    }

    #[test]
    fn anti_clogging_token() {
        let pwe = sae::pwe_hnp(b"pw", &A, &B).unwrap();
        let a = Sae::new(pwe, false, &mut rng(4));
        let t = a.token_request(&[19, 0, 1, 2, 3, 4]).unwrap();
        assert_eq!(t, [1, 2, 3, 4]);
        let c = a.commit_body(Some(&t));
        assert_eq!(&c[2..6], &[1, 2, 3, 4]);
        let pt = sae::pt_h2e(b"pw", b"s");
        let h = Sae::new(sae::pwe_from_pt(&pt, &A, &B), true, &mut rng(5));
        assert_eq!(h.token_request(&[19, 0, 255, 3, 93, 7, 8]).unwrap(), [7, 8]);
        assert!(h.token_request(&[19, 0, 7, 8]).is_err());
        let c = h.commit_body(Some(&[7, 8]));
        assert_eq!(&c[c.len() - 5..], &[255, 3, 93, 7, 8]);
    }
}

/// IEEE Std 802.11-2020 Annex J.10, as hostapd's module tests carry them.
mod sae_annex_j10 {
    use crate::sae::{self, Sae};
    use p256::elliptic_curve::sec1::ToEncodedPoint;

    const ADDR1: [u8; 6] = [0x4d, 0x3f, 0x2f, 0xff, 0xe3, 0x87];
    const ADDR2: [u8; 6] = [0xa5, 0xd8, 0xaa, 0x95, 0x8e, 0x3c];
    const ADDR1B: [u8; 6] = [0x00, 0x09, 0x5b, 0x66, 0xec, 0x1e];
    const ADDR2B: [u8; 6] = [0x00, 0x0b, 0x6b, 0xd9, 0x02, 0x46];
    const LOCAL_RAND: [u8; 32] = [
        0x99, 0x24, 0x65, 0xfd, 0x3d, 0xaa, 0x3c, 0x60, 0xaa, 0x65, 0x65, 0xb7, 0xf6, 0x2a, 0x2a, 0x7f,
        0x2e, 0x12, 0xdd, 0x12, 0xf1, 0x98, 0xfa, 0xf4, 0xfb, 0xed, 0x89, 0xd7, 0xff, 0x1a, 0xce, 0x94,
    ];
    const LOCAL_MASK: [u8; 32] = [
        0x95, 0x07, 0xa9, 0x0f, 0x77, 0x7a, 0x04, 0x4d, 0x6a, 0x08, 0x30, 0xb9, 0x1e, 0xa3, 0xd5, 0xdd,
        0x70, 0xbe, 0xce, 0x44, 0xe1, 0xac, 0xff, 0xb8, 0x69, 0x83, 0xb5, 0xe1, 0xbf, 0x9f, 0xb3, 0x22,
    ];
    const LOCAL_COMMIT: [u8; 98] = [
        0x13, 0x00, 0x2e, 0x2c, 0x0f, 0x0d, 0xb5, 0x24, 0x40, 0xad, 0x14, 0x6d, 0x96, 0x71, 0x14, 0xce,
        0x00, 0x5c, 0xe1, 0xea, 0xb0, 0xaa, 0x2c, 0x2e, 0x5c, 0x28, 0x71, 0xb7, 0x74, 0xf6, 0xc2, 0x57,
        0x5c, 0x65, 0xd5, 0xad, 0x9e, 0x00, 0x82, 0x97, 0x07, 0xaa, 0x36, 0xba, 0x8b, 0x85, 0x97, 0x38,
        0xfc, 0x96, 0x1d, 0x08, 0x24, 0x35, 0x05, 0xf4, 0x7c, 0x03, 0x53, 0x76, 0xd7, 0xac, 0x4b, 0xc8,
        0xd7, 0xb9, 0x50, 0x83, 0xbf, 0x43, 0x82, 0x7d, 0x0f, 0xc3, 0x1e, 0xd7, 0x78, 0xdd, 0x36, 0x71,
        0xfd, 0x21, 0xa4, 0x6d, 0x10, 0x91, 0xd6, 0x4b, 0x6f, 0x9a, 0x1e, 0x12, 0x72, 0x62, 0x13, 0x25,
        0xdb, 0xe1,
    ];
    const PEER_COMMIT: [u8; 98] = [
        0x13, 0x00, 0x59, 0x1b, 0x96, 0xf3, 0x39, 0x7f, 0xb9, 0x45, 0x10, 0x08, 0x48, 0xe7, 0xb5, 0x50,
        0x54, 0x3b, 0x67, 0x20, 0xd8, 0x83, 0x37, 0xee, 0x93, 0xfc, 0x49, 0xfd, 0x6d, 0xf7, 0xe0, 0x8b,
        0x52, 0x23, 0xe7, 0x1b, 0x9b, 0xb0, 0x48, 0xd3, 0x87, 0x3f, 0x20, 0x55, 0x69, 0x53, 0xa9, 0x6c,
        0x91, 0x53, 0x6f, 0xd8, 0xee, 0x6c, 0xa9, 0xb4, 0xa6, 0x8a, 0x14, 0x8b, 0x05, 0x6a, 0x90, 0x9b,
        0xe0, 0x3e, 0x83, 0xae, 0x20, 0x8f, 0x60, 0xf8, 0xef, 0x55, 0x37, 0x85, 0x80, 0x74, 0xdb, 0x06,
        0x68, 0x70, 0x32, 0x39, 0x98, 0x62, 0x99, 0x9b, 0x51, 0x1e, 0x0a, 0x15, 0x52, 0xa5, 0xfe, 0xa3,
        0x17, 0xc2,
    ];
    const KCK: [u8; 32] = [
        0x1e, 0x73, 0x3f, 0x6d, 0x9b, 0xd5, 0x32, 0x56, 0x28, 0x73, 0x04, 0x33, 0x88, 0x31, 0xb0, 0x9a,
        0x39, 0x40, 0x6d, 0x12, 0x10, 0x17, 0x07, 0x3a, 0x5c, 0x30, 0xdb, 0x36, 0xf3, 0x6c, 0xb8, 0x1a,
    ];
    const PMK: [u8; 32] = [
        0x4e, 0x4d, 0xfa, 0xb1, 0xa2, 0xdd, 0x8a, 0xc1, 0xa9, 0x17, 0x90, 0xf9, 0x53, 0xfa, 0xaa, 0x45,
        0x2a, 0xe5, 0xc6, 0x87, 0x3a, 0xb7, 0x5b, 0x63, 0x60, 0x5b, 0xa6, 0x63, 0xf8, 0xa7, 0xfe, 0x59,
    ];
    const PMKID: [u8; 16] = [
        0x87, 0x47, 0xa6, 0x00, 0xee, 0xa3, 0xf9, 0xf2, 0x24, 0x75, 0xdf, 0x58, 0xca, 0x1e, 0x54, 0x98,
    ];
    const PWE_19_X: [u8; 32] = [
        0xc9, 0x30, 0x49, 0xb9, 0xe6, 0x40, 0x00, 0xf8, 0x48, 0x20, 0x16, 0x49, 0xe9, 0x99, 0xf2, 0xb5,
        0xc2, 0x2d, 0xea, 0x69, 0xb5, 0x63, 0x2c, 0x9d, 0xf4, 0xd6, 0x33, 0xb8, 0xaa, 0x1f, 0x6c, 0x1e,
    ];
    const PWE_19_Y: [u8; 32] = [
        0x73, 0x63, 0x4e, 0x94, 0xb5, 0x3d, 0x82, 0xe7, 0x38, 0x3a, 0x8d, 0x25, 0x81, 0x99, 0xd9, 0xdc,
        0x1a, 0x5e, 0xe8, 0x26, 0x9d, 0x06, 0x03, 0x82, 0xcc, 0xbf, 0x33, 0xe6, 0x14, 0xff, 0x59, 0xa0,
    ];

    #[test]
    fn hunting_and_pecking_commit_and_keys() {
        let pwe = sae::pwe_hnp(b"mekmitasdigoat", &ADDR1, &ADDR2).unwrap();
        let mut s = Sae::with_rand_mask(pwe, false, LOCAL_RAND, LOCAL_MASK);
        assert_eq!(s.commit_body(None), LOCAL_COMMIT);
        s.rx_commit(&PEER_COMMIT).unwrap();
        assert_eq!(s.kck_for_test(), Some(KCK));
        let (pmk, pmkid) = s.keys_for_test().unwrap();
        assert_eq!(pmk, PMK);
        assert_eq!(pmkid, PMKID);
    }

    #[test]
    fn hash_to_element_pt_and_pwe() {
        // With the password identifier "psk4internet" (appended to the
        // password in the HKDF-Extract input).
        let pt = sae::pt_h2e(b"mekmitasdigoatpsk4internet", b"byteme");
        let pwe = sae::pwe_from_pt(&pt, &ADDR1B, &ADDR2B).to_affine().to_encoded_point(false);
        assert_eq!(&pwe.as_bytes()[1..33], &PWE_19_X);
        assert_eq!(&pwe.as_bytes()[33..], &PWE_19_Y);
    }
}
