//! Private one-shot wire between the trusted legacy-read fixture and its
//! fixed worker. The control channel is inherited from Portcullis.

use std::io::{self, Read, Write};

pub const MAX_CERT: usize = 4000;
pub const MAX_RECEIVED: usize = 48 << 10;
pub const INPUT_CMS: u8 = 0;
pub const INPUT_RECEIVED: u8 = 1;
pub const MAX_CMS: usize = 128 << 10;
pub const MAX_TEXT: usize = 64 << 10;
pub const WORKER_READY: &[u8] = b"legacy-jail-capmode-2";
pub const FAILURE: &str = "Couldn’t open this legacy S/MIME message.";

pub const FIXTURE_ACCOUNT: &str = "legacy-preview@fixture.invalid";

/// The fixture's exact RFC 5322 bytes. The checked-in file uses LF.
pub fn fixture_message() -> Vec<u8> {
    include_str!("../tests/data/legacy-preview.eml")
        .replace('\n', "\r\n")
        .into_bytes()
}

/// Same shape as the valid fixture with an unsupported top-level media type.
pub fn unsupported_fixture_message() -> Vec<u8> {
    include_str!("../tests/data/legacy-preview.eml")
        .replace("application/pkcs7-mime", "application/other-cmsx")
        .replace('\n', "\r\n")
        .into_bytes()
}

pub fn received_selection(bytes: &[u8]) -> Option<(Selection, &[u8])> {
    let (header, raw) = bytes.split_at_checked(Selection::SIZE)?;
    let selection = Selection::decode(header)?;
    if raw.is_empty()
        || raw.len() > MAX_RECEIVED
        || selection != Selection::for_message(selection.account, selection.message, raw)
    {
        return None;
    }
    let fixture = match selection.message {
        1 => fixture_message(),
        2 => unsupported_fixture_message(),
        _ => return None,
    };
    // Enforce the disabled gate's fixed account and uploaded fixtures.
    (selection.account == 1 && raw == fixture).then_some((selection, raw))
}

/// Snapshot of the selected local account and fully downloaded message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub account: u32,
    pub message: u64,
    pub digest: [u8; 32],
}

impl Selection {
    pub const SIZE: usize = 4 + 8 + 32;

    pub fn for_message(account: u32, message: u64, bytes: &[u8]) -> Self {
        Self {
            account,
            message,
            digest: digest(bytes),
        }
    }

    pub fn fixture() -> Self {
        // The disabled gate seeds one account and message in a fresh store.
        Self::for_message(1, 1, &fixture_message())
    }

    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[..4].copy_from_slice(&self.account.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.message.to_le_bytes());
        bytes[12..].copy_from_slice(&self.digest);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().ok()?;
        let account = u32::from_le_bytes(bytes[..4].try_into().ok()?);
        let message = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
        if account == 0 || message == 0 {
            return None;
        }
        Some(Self {
            account,
            message,
            digest: bytes[12..].try_into().ok()?,
        })
    }
}

pub fn digest(bytes: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .try_into()
        .expect("SHA-256 digest length")
}

pub fn write(out: &mut impl Write, bytes: &[u8], limit: usize) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > limit {
        return Err(io::Error::other("invalid legacy S/MIME frame size"));
    }
    out.write_all(&(bytes.len() as u32).to_le_bytes())?;
    out.write_all(bytes)
}

pub fn read(input: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut size = [0; 4];
    input.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > limit {
        return Err(io::Error::other("invalid legacy S/MIME frame size"));
    }
    let mut data = vec![0; size];
    input.read_exact(&mut data)?;
    Ok(data)
}

pub fn plain_text(decrypted: &[u8]) -> Option<&str> {
    let body = decrypted.strip_prefix(b"Content-Type: text/plain\r\n\r\n")?;
    if body.is_empty() || body.len() > MAX_TEXT {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    text.chars()
        .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        .then_some(text)
}
