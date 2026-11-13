//! EAPOL-Key frames (§12.7.2) and the KDEs in their key data.

pub const TYPE_KEY: u8 = 3;
pub const DESC_RSN: u8 = 2;

pub mod info {
    pub const VERSION: u16 = 0x0007;
    pub const PAIRWISE: u16 = 1 << 3;
    pub const INSTALL: u16 = 1 << 6;
    pub const ACK: u16 = 1 << 7;
    pub const MIC: u16 = 1 << 8;
    pub const SECURE: u16 = 1 << 9;
    pub const ERROR: u16 = 1 << 10;
    pub const REQUEST: u16 = 1 << 11;
    pub const ENCRYPTED: u16 = 1 << 12;
}

/// Offset of the MIC field in a whole EAPOL frame (16-byte MIC AKMs).
pub const MIC_OFF: usize = 81;
pub const HDR_LEN: usize = 99;

/// A parsed EAPOL-Key frame, borrowing the frame.
#[derive(Clone, Copy, Debug)]
pub struct KeyFrame<'a> {
    pub version: u8,
    pub info: u16,
    pub key_len: u16,
    pub replay: u64,
    pub nonce: &'a [u8; 32],
    pub rsc: [u8; 8],
    pub mic: &'a [u8],
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadFrame {
    Short,
    NotKey,
    Descriptor,
    Length,
}

impl<'a> KeyFrame<'a> {
    /// Parses a whole EAPOL frame. Lengths are checked against the buffer;
    /// trailing padding after the EAPOL body (from Ethernet minimums) is
    /// ignored.
    pub fn parse(f: &'a [u8]) -> Result<KeyFrame<'a>, BadFrame> {
        if f.len() < 4 {
            return Err(BadFrame::Short);
        }
        if f[1] != TYPE_KEY {
            return Err(BadFrame::NotKey);
        }
        let body_len = u16::from_be_bytes([f[2], f[3]]) as usize;
        if f.len() < 4 + body_len || body_len < HDR_LEN - 4 {
            return Err(BadFrame::Short);
        }
        if f[4] != DESC_RSN {
            return Err(BadFrame::Descriptor);
        }
        let data_len = u16::from_be_bytes([f[97], f[98]]) as usize;
        if HDR_LEN + data_len != 4 + body_len {
            return Err(BadFrame::Length);
        }
        let mut rsc = [0u8; 8];
        rsc.copy_from_slice(&f[73..81]);
        Ok(KeyFrame {
            version: f[0],
            info: u16::from_be_bytes([f[5], f[6]]),
            key_len: u16::from_be_bytes([f[7], f[8]]),
            replay: u64::from_be_bytes(f[9..17].try_into().unwrap_or([0; 8])),
            nonce: f[17..49].try_into().map_err(|_| BadFrame::Short)?,
            rsc,
            mic: &f[MIC_OFF..MIC_OFF + 16],
            data: &f[HDR_LEN..HDR_LEN + data_len],
        })
    }

    pub fn has(&self, bit: u16) -> bool {
        self.info & bit != 0
    }
}

/// Builds an EAPOL-Key frame with a zero MIC field; the caller fills the MIC
/// at [`MIC_OFF`].
pub fn build(version: u8, info: u16, key_len: u16, replay: u64, nonce: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let body = HDR_LEN - 4 + data.len();
    let mut f = Vec::with_capacity(4 + body);
    f.push(version);
    f.push(TYPE_KEY);
    f.extend_from_slice(&(body as u16).to_be_bytes());
    f.push(DESC_RSN);
    f.extend_from_slice(&info.to_be_bytes());
    f.extend_from_slice(&key_len.to_be_bytes());
    f.extend_from_slice(&replay.to_be_bytes());
    f.extend_from_slice(nonce);
    f.extend_from_slice(&[0u8; 16 + 8 + 8 + 16]); // IV, RSC, reserved, MIC
    f.extend_from_slice(&(data.len() as u16).to_be_bytes());
    f.extend_from_slice(data);
    f
}

/// A copy of `f` with the MIC field zeroed, for checking.
pub fn zero_mic(f: &[u8]) -> Vec<u8> {
    let mut v = f.to_vec();
    v[MIC_OFF..MIC_OFF + 16].fill(0);
    v
}

/// What the key data of message 3 or a group message 1 carries.
#[derive(Default)]
pub struct KeyData {
    /// The first RSNE, whole (id, length, body).
    pub rsne: Option<Vec<u8>>,
    /// A second RSNE (must be absent unless the AP offers it, §12.7.6.4).
    pub rsne2: bool,
    pub gtk: Option<(u8, Vec<u8>)>,
    pub igtk: Option<(u8, [u8; 6], Vec<u8>)>,
}

impl Drop for KeyData {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some((_, k)) = &mut self.gtk {
            k.zeroize();
        }
        if let Some((_, _, k)) = &mut self.igtk {
            k.zeroize();
        }
    }
}

const OUI: [u8; 3] = [0x00, 0x0f, 0xac];

/// Parses decrypted key data: elements and KDEs, stopping at the 0xdd
/// padding. Malformed entries fail the whole message.
pub fn parse_key_data(d: &[u8]) -> Result<KeyData, BadFrame> {
    let mut k = KeyData::default();
    let mut i = 0;
    while i < d.len() {
        let id = d[i];
        if id == 0xdd && (i + 1 == d.len() || d[i + 1] == 0) {
            break; // padding
        }
        if i + 2 > d.len() {
            return Err(BadFrame::Length);
        }
        let n = d[i + 1] as usize;
        if i + 2 + n > d.len() {
            return Err(BadFrame::Length);
        }
        let body = &d[i + 2..i + 2 + n];
        match id {
            48 => {
                if k.rsne.is_some() {
                    k.rsne2 = true;
                } else {
                    k.rsne = Some(d[i..i + 2 + n].to_vec());
                }
            }
            0xdd if n >= 4 && body[..3] == OUI => {
                let kd = &body[4..];
                match body[3] {
                    1 => {
                        // GTK: key id (bits 0-1), Tx, reserved, GTK.
                        if kd.len() < 2 + 16 || kd.len() > 2 + 32 || k.gtk.is_some() {
                            return Err(BadFrame::Length);
                        }
                        k.gtk = Some((kd[0] & 3, kd[2..].to_vec()));
                    }
                    9 => {
                        // IGTK: key id (u16 LE), IPN (6), IGTK.
                        if kd.len() < 8 + 16 || kd.len() > 8 + 32 || k.igtk.is_some() {
                            return Err(BadFrame::Length);
                        }
                        let id = u16::from_le_bytes([kd[0], kd[1]]);
                        if !(4..=5).contains(&id) {
                            return Err(BadFrame::Length);
                        }
                        let mut ipn = [0u8; 6];
                        ipn.copy_from_slice(&kd[2..8]);
                        k.igtk = Some((id as u8, ipn, kd[8..].to_vec()));
                    }
                    _ => {} // other KDEs ignored
                }
            }
            _ => {} // other elements ignored
        }
        i += 2 + n;
    }
    Ok(k)
}
