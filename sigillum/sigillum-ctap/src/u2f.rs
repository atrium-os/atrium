//! CTAP1/U2F (FIDO U2F raw messages 1.2) over `CTAPHID_MSG`, mapped onto
//! WebAuthn the way CTAP 2.1 §10 says, for keys with no CTAP2 (old keys,
//! and QEMU's `u2f-emulated`, which the VM gate uses).
//!
//! - Register: `00 01 00 00 | ext Lc | cdh[32] | SHA-256(rpId)[32] | 00 00`
//!   → `05 | pubkey[65] | L | keyHandle[L] | cert | sig | 90 00`.
//! - Authenticate: `00 02 P1 00 | ext Lc | cdh | appId | L | keyHandle | 00 00`
//!   → `UP[1] | counter[4] | sig | 90 00`. P1 3 = check presence and
//!   sign, 7 = only check the handle is ours.
//! - `69 85`: no presence yet (poll); `6A 80`: not this key's handle.
//!
//! A U2F signature is over `appId | UP | counter | cdh`, which is exactly
//! WebAuthn's `authenticatorData (37 bytes) | clientDataHash`, so the RP
//! verifies it unchanged.

use crate::Error;

pub const REGISTER: u8 = 0x01;
pub const AUTHENTICATE: u8 = 0x02;
pub const VERSION: u8 = 0x03;
pub const P1_ENFORCE: u8 = 0x03;
pub const P1_CHECK_ONLY: u8 = 0x07;

pub mod sw {
    pub const OK: u16 = 0x9000;
    pub const CONDITIONS_NOT_SATISFIED: u16 = 0x6985;
    pub const WRONG_DATA: u16 = 0x6a80;
}

/// An extended-length command APDU.
pub fn apdu(ins: u8, p1: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![0x00, ins, p1, 0x00];
    if !data.is_empty() {
        v.push(0);
        v.extend_from_slice(&(data.len() as u16).to_be_bytes());
        v.extend_from_slice(data);
    } else {
        v.push(0);
    }
    v.extend_from_slice(&[0x00, 0x00]);
    v
}

/// Split a response APDU into (body, status word).
pub fn split(resp: &[u8]) -> Result<(&[u8], u16), Error> {
    if resp.len() < 2 {
        return Err(Error::Protocol("short U2F response"));
    }
    let n = resp.len() - 2;
    Ok((&resp[..n], u16::from_be_bytes([resp[n], resp[n + 1]])))
}

pub fn register_request(cdh: &[u8; 32], app: &[u8; 32]) -> Vec<u8> {
    let mut d = cdh.to_vec();
    d.extend_from_slice(app);
    apdu(REGISTER, 0, &d)
}

pub fn authenticate_request(p1: u8, cdh: &[u8; 32], app: &[u8; 32], handle: &[u8]) -> Result<Vec<u8>, Error> {
    if handle.is_empty() || handle.len() > 255 {
        return Err(Error::Request("U2F key handle length"));
    }
    let mut d = cdh.to_vec();
    d.extend_from_slice(app);
    d.push(handle.len() as u8);
    d.extend_from_slice(handle);
    Ok(apdu(AUTHENTICATE, p1, &d))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// SEC1 uncompressed P-256 point.
    pub public_key: [u8; 65],
    pub key_handle: Vec<u8>,
}

/// The register body (without the status word). The attestation
/// certificate and signature that follow are not needed under
/// attestation `none` and are not parsed.
pub fn parse_registration(b: &[u8]) -> Result<Registration, Error> {
    if b.len() < 67 || b[0] != 0x05 || b[1] != 0x04 {
        return Err(Error::Protocol("U2F registration header"));
    }
    let public_key: [u8; 65] = b[1..66].try_into().unwrap();
    let n = b[66] as usize;
    if n == 0 || b.len() < 67 + n {
        return Err(Error::Protocol("U2F key handle"));
    }
    Ok(Registration { public_key, key_handle: b[67..67 + n].to_vec() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authentication {
    pub up: u8,
    pub counter: u32,
    pub signature: Vec<u8>,
}

pub fn parse_authentication(b: &[u8]) -> Result<Authentication, Error> {
    if b.len() < 6 || b.len() > 5 + 80 {
        return Err(Error::Protocol("U2F authentication length"));
    }
    Ok(Authentication { up: b[0], counter: u32::from_be_bytes(b[1..5].try_into().unwrap()), signature: b[5..].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apdus() {
        let a = authenticate_request(P1_ENFORCE, &[1; 32], &[2; 32], &[9; 3]).unwrap();
        assert_eq!(&a[..7], &[0, 2, 3, 0, 0, 0, 68]);
        assert_eq!(a.len(), 7 + 68 + 2);
        assert_eq!(split(&[1, 2, 0x69, 0x85]).unwrap(), (&[1u8, 2][..], sw::CONDITIONS_NOT_SATISFIED));
        assert!(parse_registration(&[5; 10]).is_err());
        assert!(parse_authentication(&[1, 0, 0, 0, 1]).is_err());
    }
}
