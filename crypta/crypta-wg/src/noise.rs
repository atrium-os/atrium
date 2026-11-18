//! Noise_IKpsk2 as WireGuard uses it (protocol page, "First Message" and
//! "Second Message"). Every DH with our static key goes through
//! [`StaticKey`]; ephemeral keys are made and used here.

use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::{self, hash, kdf1, kdf2, kdf3, mac, Key};
use crate::key::{dh, KeyError, StaticKey};
use crate::messages::{self, Initiation, Response};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HsError {
    Key(KeyError),
    /// A sealed field did not open: not for us, or tampered with.
    Decrypt,
    /// A field was well formed but wrong (an unknown static key).
    Refused(&'static str),
}

impl From<KeyError> for HsError {
    fn from(e: KeyError) -> Self {
        HsError::Key(e)
    }
}

fn initial() -> (Key, [u8; 32]) {
    let c = hash(&[crypto::CONSTRUCTION]);
    let h = hash(&[&c, crypto::IDENTIFIER]);
    (Zeroizing::new(c), h)
}

fn mix_hash(h: &mut [u8; 32], data: &[u8]) {
    *h = hash(&[&h[..], data]);
}

/// The key mac1 is made with for messages to the holder of `public`.
pub fn mac1_key(public: &[u8; 32]) -> [u8; 32] {
    hash(&[crypto::LABEL_MAC1, public])
}

/// The key cookie replies from the holder of `public` are sealed with.
pub fn cookie_key(public: &[u8; 32]) -> [u8; 32] {
    hash(&[crypto::LABEL_COOKIE, public])
}

/// Fill mac1 (and mac2 if we hold a cookie) of an encoded handshake
/// message whose mac1 starts at `at`.
pub fn add_macs(msg: &mut [u8], at: usize, mac1_key: &[u8; 32], cookie: Option<&[u8; 16]>) -> [u8; 16] {
    let m1 = mac(mac1_key, &[&msg[..at]]);
    msg[at..at + 16].copy_from_slice(&m1);
    let m2 = match cookie {
        Some(c) => mac(c, &[&msg[..at + 16]]),
        None => [0u8; 16],
    };
    msg[at + 16..at + 32].copy_from_slice(&m2);
    m1
}

/// Whether a handshake message's mac1 is right for `mac1_key` (ours).
pub fn mac1_ok(msg: &[u8], at: usize, mac1_key: &[u8; 32]) -> bool {
    crypto::eq(&mac(mac1_key, &[&msg[..at]]), &msg[at..at + 16])
}

/// Whether its mac2 is right for `cookie`.
pub fn mac2_ok(msg: &[u8], at: usize, cookie: &[u8; 16]) -> bool {
    crypto::eq(&mac(cookie, &[&msg[..at + 16]]), &msg[at + 16..at + 32])
}

/// The transport keys of one handshake.
pub struct Keypair {
    pub send: Key,
    pub recv: Key,
    pub local_index: u32,
    pub remote_index: u32,
    /// We sent the initiation: we may send at once; the responder waits for
    /// our first packet (key confirmation).
    pub initiator: bool,
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keypair {{ local {}, remote {}, initiator {} }}", self.local_index, self.remote_index, self.initiator)
    }
}

/// An initiation we sent, waiting for its response.
pub struct InitiatorState {
    ck: Key,
    h: [u8; 32],
    eph: StaticSecret,
    pub sender: u32,
    /// mac1 of the initiation, for opening a cookie reply to it.
    pub mac1: [u8; 16],
}

impl Drop for InitiatorState {
    fn drop(&mut self) {
        self.h.zeroize();
    }
}

/// Make an initiation to `peer` from `local`, with `sender` as our index.
pub fn create_initiation(
    local: &dyn StaticKey,
    peer: &[u8; 32],
    sender: u32,
    timestamp: [u8; 12],
    cookie: Option<&[u8; 16]>,
) -> Result<([u8; messages::INITIATION_LEN], InitiatorState), HsError> {
    let (ck, mut h) = initial();
    mix_hash(&mut h, peer);
    let eph = ephemeral();
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let ck = kdf1(&ck[..], &eph_pub);
    mix_hash(&mut h, &eph_pub);
    let (ck, k) = kdf2(&ck[..], &dh(&eph, peer)?[..]);
    let static_sealed: [u8; 48] = crypto::seal_vec(&k, 0, &h, &local.public()).try_into().expect("48");
    mix_hash(&mut h, &static_sealed);
    let (ck, k) = kdf2(&ck[..], &local.dh(peer)?[..]);
    let ts_sealed: [u8; 28] = crypto::seal_vec(&k, 0, &h, &timestamp).try_into().expect("28");
    mix_hash(&mut h, &ts_sealed);
    let mut msg = Initiation { sender, ephemeral: eph_pub, static_sealed, timestamp_sealed: ts_sealed, mac1: [0; 16], mac2: [0; 16] }.encode();
    let mac1 = add_macs(&mut msg, messages::INITIATION_MAC1, &mac1_key(peer), cookie);
    Ok((msg, InitiatorState { ck, h, eph, sender, mac1 }))
}

/// An initiation we received and opened: who it is from, and the state to
/// answer it with.
pub struct ConsumedInitiation {
    pub peer_static: [u8; 32],
    pub timestamp: [u8; 12],
    pub peer_index: u32,
    ck: Key,
    h: [u8; 32],
    peer_eph: [u8; 32],
}

/// Open an initiation sent to `local` (mac1 already checked).
pub fn consume_initiation(local: &dyn StaticKey, msg: &Initiation) -> Result<ConsumedInitiation, HsError> {
    let (ck, mut h) = initial();
    mix_hash(&mut h, &local.public());
    let ck = kdf1(&ck[..], &msg.ephemeral);
    mix_hash(&mut h, &msg.ephemeral);
    let (ck, k) = kdf2(&ck[..], &local.dh(&msg.ephemeral)?[..]);
    let peer = crypto::open_vec(&k, 0, &h, &msg.static_sealed).ok_or(HsError::Decrypt)?;
    let peer_static: [u8; 32] = peer[..].try_into().map_err(|_| HsError::Decrypt)?;
    mix_hash(&mut h, &msg.static_sealed);
    let (ck, k) = kdf2(&ck[..], &local.dh(&peer_static)?[..]);
    let ts = crypto::open_vec(&k, 0, &h, &msg.timestamp_sealed).ok_or(HsError::Decrypt)?;
    let timestamp: [u8; 12] = ts[..].try_into().map_err(|_| HsError::Decrypt)?;
    mix_hash(&mut h, &msg.timestamp_sealed);
    Ok(ConsumedInitiation { peer_static, timestamp, peer_index: msg.sender, ck, h, peer_eph: msg.ephemeral })
}

/// Answer an opened initiation: the response and the new keypair.
pub fn create_response(
    c: &ConsumedInitiation,
    psk: &[u8; 32],
    sender: u32,
    cookie: Option<&[u8; 16]>,
) -> Result<([u8; messages::RESPONSE_LEN], Keypair), HsError> {
    let mut h = c.h;
    let eph = ephemeral();
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let ck = kdf1(&c.ck[..], &eph_pub);
    mix_hash(&mut h, &eph_pub);
    let ck = kdf1(&ck[..], &dh(&eph, &c.peer_eph)?[..]);
    let ck = kdf1(&ck[..], &dh(&eph, &c.peer_static)?[..]);
    let (ck, t, k) = kdf3(&ck[..], psk);
    mix_hash(&mut h, &t[..]);
    let empty: [u8; 16] = crypto::seal_vec(&k, 0, &h, &[]).try_into().expect("16");
    mix_hash(&mut h, &empty);
    let mut msg = Response { sender, receiver: c.peer_index, ephemeral: eph_pub, empty_sealed: empty, mac1: [0; 16], mac2: [0; 16] }.encode();
    add_macs(&mut msg, messages::RESPONSE_MAC1, &mac1_key(&c.peer_static), cookie);
    let (recv, send) = kdf2(&ck[..], &[]);
    h.zeroize();
    Ok((msg, Keypair { send, recv, local_index: sender, remote_index: c.peer_index, initiator: false }))
}

/// Open the response to our initiation `st` (mac1 already checked).
pub fn consume_response(local: &dyn StaticKey, st: &InitiatorState, psk: &[u8; 32], msg: &Response) -> Result<Keypair, HsError> {
    if msg.receiver != st.sender {
        return Err(HsError::Refused("not the initiation we sent"));
    }
    let mut h = st.h;
    let ck = kdf1(&st.ck[..], &msg.ephemeral);
    mix_hash(&mut h, &msg.ephemeral);
    let ck = kdf1(&ck[..], &dh(&st.eph, &msg.ephemeral)?[..]);
    let ck = kdf1(&ck[..], &local.dh(&msg.ephemeral)?[..]);
    let (ck, t, k) = kdf3(&ck[..], psk);
    mix_hash(&mut h, &t[..]);
    crypto::open_vec(&k, 0, &h, &msg.empty_sealed).ok_or(HsError::Decrypt)?;
    let (send, recv) = kdf2(&ck[..], &[]);
    h.zeroize();
    Ok(Keypair { send, recv, local_index: st.sender, remote_index: msg.sender, initiator: true })
}

/// A fresh ephemeral key.
fn ephemeral() -> StaticSecret {
    let mut b = crypto::random::<32>();
    let k = StaticSecret::from(b);
    b.zeroize();
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::LocalKey;
    use crate::messages::{parse, Message};

    #[test]
    fn a_handshake_gives_both_sides_the_same_keys() {
        let a = LocalKey::generate();
        let b = LocalKey::generate();
        let psk = [9u8; 32];
        let ts = crypto::tai64n(std::time::Duration::from_secs(1_700_000_000));
        let (init, st) = create_initiation(&a, &b.public(), 11, ts, None).unwrap();
        assert!(mac1_ok(&init, messages::INITIATION_MAC1, &mac1_key(&b.public())));
        assert!(!mac1_ok(&init, messages::INITIATION_MAC1, &mac1_key(&a.public())));
        let Some(Message::Initiation(i)) = parse(&init) else { panic!() };
        let c = consume_initiation(&b, &i).unwrap();
        assert_eq!(c.peer_static, a.public());
        assert_eq!(c.timestamp, ts);
        let (resp, kb) = create_response(&c, &psk, 22, None).unwrap();
        assert!(mac1_ok(&resp, messages::RESPONSE_MAC1, &mac1_key(&a.public())));
        let Some(Message::Response(r)) = parse(&resp) else { panic!() };
        let ka = consume_response(&a, &st, &psk, &r).unwrap();
        assert_eq!(ka.send[..], kb.recv[..]);
        assert_eq!(ka.recv[..], kb.send[..]);
        assert_ne!(ka.send[..], ka.recv[..]);
        assert_eq!((ka.local_index, ka.remote_index, kb.local_index, kb.remote_index), (11, 22, 22, 11));
        // The wrong preshared key: the response does not open.
        assert_eq!(consume_response(&a, &st, &[0u8; 32], &r).unwrap_err(), HsError::Decrypt);
    }

    #[test]
    fn an_initiation_for_someone_else_does_not_open() {
        let a = LocalKey::generate();
        let b = LocalKey::generate();
        let c = LocalKey::generate();
        let (init, _) = create_initiation(&a, &b.public(), 1, [0; 12], None).unwrap();
        let Some(Message::Initiation(i)) = parse(&init) else { panic!() };
        assert!(matches!(consume_initiation(&c, &i), Err(HsError::Decrypt)));
        let mut t = i.clone();
        t.timestamp_sealed[3] ^= 1;
        assert!(matches!(consume_initiation(&b, &t), Err(HsError::Decrypt)));
    }
}
