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
    fn vpn_document_bounds_both_bodies_and_consumes_the_frame() {
        let doc = VpnDocument { html: b"<title>Sign in</title>", script: b"document.forms[0].requestSubmit()" };
        let mut wire = doc.encode().unwrap();
        assert_eq!(VpnDocument::decode(&wire).unwrap().script, doc.script);
        assert!(VpnDocument::decode(&wire[..wire.len() - 1]).is_none());
        wire.push(0);
        assert!(VpnDocument::decode(&wire).is_none());
        let too_large = vec![0; 65537];
        let oversized = VpnDocument { script: &too_large, ..doc };
        assert!(VpnDocument::decode(&oversized.encode().unwrap()).is_none());
    }

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

/// The VPN fixture's script is fetched through the current profile's courier.
/// The sealed file contains both responses, with no URL authority for the worker.
pub const VPN_JS_LOGIN: &str = "https://sso.crypta.test:447/+CSCOE+/saml/sp/login";
pub const VPN_JS_PATH: &str = "/+CSCOE+/saml/sp/continue.js";
pub const VPN_JS_URL: &str = "https://sso.crypta.test:447/+CSCOE+/saml/sp/continue.js";

#[derive(Serialize, Deserialize)]
pub struct VpnDocument<'a> {
    pub html: &'a [u8],
    pub script: &'a [u8],
}

impl VpnDocument<'_> {
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> { postcard::to_stdvec(self) }

    pub fn decode(bytes: &[u8]) -> Option<VpnDocument<'_>> {
        let (doc, rest) = postcard::take_from_bytes::<VpnDocument>(bytes).ok()?;
        (rest.is_empty() && !doc.html.is_empty() && doc.html.len() <= 2 << 20 && doc.script.len() <= 65536).then_some(doc)
    }
}
