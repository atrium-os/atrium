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
    /// A VPN sign-in page asks for a script or stylesheet at `url` (vpn.md
    /// "Scripts in the sign-in window"). The window forwards it to the
    /// broker, which fetches only the delivered document's own HTTPS
    /// origin through the profile's couriers. The answer is
    /// [`ResourceReady`] with a sealed file, or `ResourceFailed`.
    Resource { id: u32, url: &'a str },
    ResourceFailed { id: u32 },
}

/// What a VPN page's resource is, as the broker read its content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    Script,
    Stylesheet,
}

/// The longest resource the broker hands a VPN page.
pub const VPN_RESOURCE_MAX: usize = 1 << 20;
/// The longest address a VPN page asks for.
pub const VPN_RESOURCE_URL_MAX: usize = 2048;

/// The bytes that go with a resource's sealed file: which request it
/// answers and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceReady {
    pub id: u32,
    pub kind: ResourceKind,
}

const READY_PREFIX: &[u8] = b"\0Resource\x01";

impl ResourceReady {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = READY_PREFIX.to_vec();
        bytes.extend(postcard::to_stdvec(self).expect("encodes"));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<ResourceReady> {
        let (ready, rest) = postcard::take_from_bytes(bytes.strip_prefix(READY_PREFIX)?).ok()?;
        rest.is_empty().then_some(ready)
    }
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

    #[test]
    fn a_resource_request_and_its_answer_round_trip_and_stay_apart() {
        let ask = CredentialMessage::Resource { id: 7, url: "https://idp.example/realms/x/js/a.js" };
        assert_eq!(decode(&ask.encode().unwrap()), Some(ask));
        let ready = ResourceReady { id: 7, kind: ResourceKind::Script };
        let wire = ready.encode();
        assert_eq!(ResourceReady::decode(&wire), Some(ready));
        assert!(decode(&wire).is_none(), "a resource answer is not a credential message");
        assert!(ResourceReady::decode(&wire[..wire.len() - 1]).is_none());
        let mut long = wire.clone();
        long.push(0);
        assert!(ResourceReady::decode(&long).is_none());
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
