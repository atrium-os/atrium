//! Credential messages on the document's existing sign-in embed. The broker
//! obtains the origin from the delivered document. No request carries one.
use serde::{Deserialize, Serialize};

const PREFIX: &[u8] = b"\0WebAuthn\x01";

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum CredentialMessage<'a> {
    Get { id: u32, options: &'a str },
    Abort { id: u32 },
    Assertion { id: u32, json: &'a str },
    SubmitFixture { body: &'a str },
    Error { id: u32, name: &'a str },
    Create { id: u32, options: &'a str },
    Registration { id: u32, json: &'a str },
}

impl CredentialMessage<'_> {
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut bytes = PREFIX.to_vec();
        bytes.extend(postcard::to_stdvec(self).ok()?);
        (bytes.len() <= crate::catalogue::MAX_MESSAGE).then_some(bytes)
    }
}

pub fn decode(bytes: &[u8]) -> Option<CredentialMessage<'_>> {
    if bytes.len() > crate::catalogue::MAX_MESSAGE {
        return None;
    }
    let (message, rest) = postcard::take_from_bytes(bytes.strip_prefix(PREFIX)?).ok()?;
    rest.is_empty().then_some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incomplete_extended_and_oversized_frames() {
        let request = CredentialMessage::Get {
            id: 4,
            options: "{}",
        };
        let mut wire = request.encode().unwrap();
        assert_eq!(decode(&wire), Some(request));
        assert!(decode(&wire[..wire.len() - 1]).is_none());
        wire.push(0);
        assert!(decode(&wire).is_none());
        wire.resize(crate::catalogue::MAX_MESSAGE + 1, 0);
        assert!(decode(&wire).is_none());
        assert!(decode(b"https://login.microsoftonline.com/first").is_none());
    }
}
