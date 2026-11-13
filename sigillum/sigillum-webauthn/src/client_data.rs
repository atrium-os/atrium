//! `clientDataJSON`, built by the broker from trusted values only, with
//! WebAuthn L3 §5.8.1.1's limited serialization (`CCDToString`): a fixed
//! key order, so a relying party may also verify it by template.

use sha2::{Digest, Sha256};

use crate::origin::Origin;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Create,
    Get,
}

/// The JSON for one ceremony. `challenge` is the page's bytes; `origin`
/// is the broker's; `top_origin` is set only for a cross-origin frame.
pub fn client_data_json(ty: Type, challenge: &[u8], origin: &Origin, top_origin: Option<&Origin>) -> String {
    let ty = match ty {
        Type::Create => "webauthn.create",
        Type::Get => "webauthn.get",
    };
    let mut s = String::from("{\"type\":");
    ccd_string(&mut s, ty);
    s.push_str(",\"challenge\":");
    ccd_string(&mut s, &crate::b64url(challenge));
    s.push_str(",\"origin\":");
    ccd_string(&mut s, &origin.serialize());
    s.push_str(",\"crossOrigin\":");
    s.push_str(if top_origin.is_some() { "true" } else { "false" });
    if let Some(t) = top_origin {
        s.push_str(",\"topOrigin\":");
        ccd_string(&mut s, &t.serialize());
    }
    s.push('}');
    s
}

pub fn hash(client_data_json: &str) -> [u8; 32] {
    Sha256::digest(client_data_json.as_bytes()).into()
}

/// `CCDToString`: a JSON string, escaping `"`, `\` and controls as `\uXXXX`.
fn ccd_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_order_and_values() {
        let o = Origin::parse("https://login.example.com").unwrap();
        assert_eq!(
            client_data_json(Type::Create, b"\x01\x02\x03", &o, None),
            r#"{"type":"webauthn.create","challenge":"AQID","origin":"https://login.example.com","crossOrigin":false}"#
        );
        let top = Origin::parse("https://shop.example").unwrap();
        assert_eq!(
            client_data_json(Type::Get, b"", &o, Some(&top)),
            r#"{"type":"webauthn.get","challenge":"","origin":"https://login.example.com","crossOrigin":true,"topOrigin":"https://shop.example"}"#
        );
        let mut s = String::new();
        ccd_string(&mut s, "a\"b\\c\n");
        assert_eq!(s, r#""a\"b\\c\u000a""#);
    }
}
