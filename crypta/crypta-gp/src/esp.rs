//! GlobalProtect ESP in UDP, using RFC 4303, RFC 3602 and RFC 2404.
//! A security association is directional. Callers must replace both
//! associations when refreshing gateway key material.

use aes::Aes128;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use zeroize::Zeroizing;

const BLOCK: usize = 16;
const HEADER: usize = 8;
const TAG: usize = 12;
const IPV4: u8 = 4;
pub const MAX_WIRE: usize = HEADER + BLOCK + 1520 + TAG;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadPacket,
    Authentication,
    Replay,
    Exhausted,
    Entropy,
}

pub struct Key {
    spi: u32,
    encryption: Zeroizing<[u8; 16]>,
    authentication: Zeroizing<[u8; 20]>,
}

impl Key {
    pub fn new(
        spi: u32,
        encryption: Zeroizing<[u8; 16]>,
        authentication: Zeroizing<[u8; 20]>,
    ) -> Option<Self> {
        // RFC 4303 reserves SPI 0 through 255.
        (spi > 255).then_some(Self {
            spi,
            encryption,
            authentication,
        })
    }

    pub fn spi(&self) -> u32 {
        self.spi
    }

    fn mac(&self, message: &[u8]) -> Hmac<Sha1> {
        let mut mac =
            Hmac::<Sha1>::new_from_slice(&self.authentication[..]).expect("fixed HMAC key");
        mac.update(message);
        mac
    }
}

pub struct Send {
    key: Key,
    sequence: u32,
}

impl Send {
    pub fn new(key: Key) -> Self {
        Self { key, sequence: 0 }
    }

    pub fn packet(&mut self, ip: &[u8]) -> Result<Vec<u8>, Error> {
        let mut iv = [0; BLOCK];
        getrandom::getrandom(&mut iv).map_err(|_| Error::Entropy)?;
        self.seal(ip, iv)
    }

    fn seal(&mut self, ip: &[u8], iv: [u8; BLOCK]) -> Result<Vec<u8>, Error> {
        if !ipv4(ip) {
            return Err(Error::BadPacket);
        }
        let sequence = self.sequence.checked_add(1).ok_or(Error::Exhausted)?;
        let padding = (BLOCK - (ip.len() + 2) % BLOCK) % BLOCK;
        let mut out = Vec::with_capacity(HEADER + BLOCK + ip.len() + padding + 2 + TAG);
        out.extend_from_slice(&self.key.spi.to_be_bytes());
        out.extend_from_slice(&sequence.to_be_bytes());
        out.extend_from_slice(&iv);
        out.extend_from_slice(ip);
        out.extend((1..=padding).map(|i| i as u8));
        out.extend([padding as u8, IPV4]);
        let data = &mut out[HEADER + BLOCK..];
        let len = data.len();
        cbc::Encryptor::<Aes128>::new((&*self.key.encryption).into(), (&iv).into())
            .encrypt_padded_mut::<NoPadding>(data, len)
            .map_err(|_| Error::BadPacket)?;
        let tag = self.key.mac(&out).finalize().into_bytes();
        out.extend_from_slice(&tag[..TAG]);
        self.sequence = sequence;
        Ok(out)
    }
}

pub struct Receive {
    key: Key,
    newest: u32,
    seen: u64,
}

impl Receive {
    pub fn new(key: Key) -> Self {
        Self {
            key,
            newest: 0,
            seen: 0,
        }
    }

    pub fn packet(&mut self, wire: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        if !(HEADER + 2 * BLOCK + TAG..=MAX_WIRE).contains(&wire.len())
            || (wire.len() - HEADER - BLOCK - TAG) % BLOCK != 0
            || wire[..4] != self.key.spi.to_be_bytes()
        {
            return Err(Error::BadPacket);
        }
        let seq = u32::from_be_bytes(wire[4..8].try_into().unwrap());
        let behind = self.newest.saturating_sub(seq);
        if seq == 0 || behind >= 64 || (seq <= self.newest && self.seen & (1 << behind) != 0) {
            return Err(Error::Replay);
        }
        let (message, tag) = wire.split_at(wire.len() - TAG);
        self.key
            .mac(message)
            .verify_truncated_left(tag)
            .map_err(|_| Error::Authentication)?;
        let iv: &[u8; BLOCK] = message[HEADER..HEADER + BLOCK].try_into().unwrap();
        let mut plain = Zeroizing::new(message[HEADER + BLOCK..].to_vec());
        cbc::Decryptor::<Aes128>::new((&*self.key.encryption).into(), iv.into())
            .decrypt_padded_mut::<NoPadding>(&mut plain)
            .map_err(|_| Error::BadPacket)?;
        let pad_at = plain.len() - 2;
        let padding = usize::from(plain[pad_at]);
        if plain[pad_at + 1] != IPV4 || padding >= BLOCK || padding > pad_at {
            return Err(Error::BadPacket);
        }
        let end = pad_at - padding;
        if !ipv4(&plain[..end])
            || plain[end..pad_at]
                .iter()
                .enumerate()
                .any(|(i, b)| usize::from(*b) != i + 1)
        {
            return Err(Error::BadPacket);
        }
        if seq > self.newest {
            self.seen = self.seen.checked_shl(seq - self.newest).unwrap_or_default() | 1;
            self.newest = seq;
        } else {
            self.seen |= 1 << behind;
        }
        plain.truncate(end);
        Ok(plain)
    }
}

fn ipv4(packet: &[u8]) -> bool {
    if !(20..=super::ssl::MAX_PACKET).contains(&packet.len()) || packet[0] >> 4 != 4 {
        return false;
    }
    let header = usize::from(packet[0] & 15) * 4;
    header >= 20
        && header <= packet.len()
        && usize::from(u16::from_be_bytes([packet[2], packet[3]])) == packet.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::new(
            0x01234567,
            Zeroizing::new([0x11; 16]),
            Zeroizing::new([0x22; 20]),
        )
        .unwrap()
    }

    const PACKET: [u8; 20] = [
        0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
    ];

    #[test]
    fn authenticates_before_advancing_the_replay_window() {
        let mut send = Send::new(key());
        let mut receive = Receive::new(key());
        let first = send.seal(&PACKET, [0x33; 16]).unwrap();
        let second = send.seal(&PACKET, [0x44; 16]).unwrap();
        let mut forged = second.clone();
        forged[7] = 100;
        assert!(matches!(
            receive.packet(&forged),
            Err(Error::Authentication)
        ));
        assert_eq!(&receive.packet(&second).unwrap()[..], &PACKET);
        assert_eq!(&receive.packet(&first).unwrap()[..], &PACKET);
        assert!(matches!(receive.packet(&first), Err(Error::Replay)));
        for _ in 0..64 {
            let latest = send.seal(&PACKET, [0x55; 16]).unwrap();
            receive.packet(&latest).unwrap();
        }
        assert!(matches!(receive.packet(&second), Err(Error::Replay)));
    }

    #[test]
    fn sequence_exhaustion_never_wraps() {
        let mut send = Send {
            key: key(),
            sequence: u32::MAX,
        };
        assert_eq!(send.seal(&PACKET, [0; 16]), Err(Error::Exhausted));
    }
}
