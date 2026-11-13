//! The page's options, in WebAuthn L3's JSON forms
//! (`PublicKeyCredentialCreationOptionsJSON`,
//! `PublicKeyCredentialRequestOptionsJSON`): binary fields as base64url.
//! Untrusted: every field is bounded here, before anything is asked.

use serde::Deserialize;

use crate::{unb64url, DomError};

/// COSE ES256, the one algorithm every authenticator we drive has.
pub const ES256: i64 = -7;

/// Longest challenge taken (the spec asks for at least 16 random bytes;
/// nothing needs more than this).
pub const MAX_CHALLENGE: usize = 1024;
/// WebAuthn caps the user handle at 64 bytes.
pub const MAX_USER_ID: usize = 64;
/// Credential ids per list.
pub const MAX_CREDENTIALS: usize = 64;
/// Credential id length (WebAuthn: at most 1023 bytes).
pub const MAX_CRED_ID: usize = 1023;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpJson {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserJson {
    pub id: String,
    pub name: String,
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ParamJson {
    #[serde(rename = "type")]
    pub ty: String,
    pub alg: i64,
}

#[derive(Debug, Deserialize)]
pub struct DescriptorJson {
    #[serde(rename = "type")]
    pub ty: String,
    pub id: String,
    #[serde(default)]
    pub transports: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionJson {
    pub authenticator_attachment: Option<String>,
    pub resident_key: Option<String>,
    #[serde(default)]
    pub require_resident_key: bool,
    pub user_verification: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreationJson {
    pub rp: RpJson,
    pub user: UserJson,
    pub challenge: String,
    pub pub_key_cred_params: Vec<ParamJson>,
    pub timeout: Option<u64>,
    #[serde(default)]
    pub exclude_credentials: Vec<DescriptorJson>,
    #[serde(default)]
    pub authenticator_selection: SelectionJson,
    pub attestation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestJson {
    pub challenge: String,
    pub timeout: Option<u64>,
    pub rp_id: Option<String>,
    #[serde(default)]
    pub allow_credentials: Vec<DescriptorJson>,
    pub user_verification: Option<String>,
}

/// User verification, as asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uv {
    Required,
    Preferred,
    Discouraged,
}

fn uv(s: Option<&str>) -> Uv {
    // Unknown values are ignored, as the spec says for enums in dictionaries.
    match s {
        Some("required") => Uv::Required,
        Some("discouraged") => Uv::Discouraged,
        _ => Uv::Preferred,
    }
}

/// `create()` options, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationOptions {
    pub rp_id: Option<String>,
    pub rp_name: String,
    pub user_id: Vec<u8>,
    pub user_name: String,
    pub user_display_name: String,
    pub challenge: Vec<u8>,
    pub timeout_ms: u32,
    pub exclude: Vec<Vec<u8>>,
    pub resident_key: bool,
    pub uv: Uv,
    /// The page asked for `direct` or `enterprise` attestation. We answer
    /// `none` unless the user agrees on the sheet (§9.3; not built yet).
    pub wants_attestation: bool,
}

/// `get()` options, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    pub rp_id: Option<String>,
    pub challenge: Vec<u8>,
    pub timeout_ms: u32,
    pub allow: Vec<Vec<u8>>,
    pub uv: Uv,
}

fn challenge(s: &str) -> Result<Vec<u8>, DomError> {
    let c = unb64url(s).ok_or(DomError::Type)?;
    if c.is_empty() || c.len() > MAX_CHALLENGE {
        return Err(DomError::Type);
    }
    Ok(c)
}

/// Public-key descriptors; other types are skipped, as the spec says.
fn descriptors(list: &[DescriptorJson]) -> Result<Vec<Vec<u8>>, DomError> {
    if list.len() > MAX_CREDENTIALS {
        return Err(DomError::Type);
    }
    let mut out = Vec::new();
    for d in list.iter().filter(|d| d.ty == "public-key") {
        let id = unb64url(&d.id).ok_or(DomError::Type)?;
        if id.is_empty() || id.len() > MAX_CRED_ID {
            return Err(DomError::Type);
        }
        out.push(id);
    }
    Ok(out)
}

fn timeout(t: Option<u64>) -> u32 {
    t.map_or(0, |t| t.min(u32::MAX as u64) as u32)
}

/// A display string: at most 64 bytes, as authenticators store them
/// (WebAuthn §6.4.1 lets the client truncate), cut on a char boundary.
fn display(s: &str) -> String {
    let mut end = s.len().min(64);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

impl CreationOptions {
    pub fn from_json(json: &str) -> Result<CreationOptions, DomError> {
        if json.len() > 64 * 1024 {
            return Err(DomError::Type);
        }
        let j: CreationJson = serde_json::from_str(json).map_err(|_| DomError::Type)?;
        let user_id = unb64url(&j.user.id).ok_or(DomError::Type)?;
        if user_id.is_empty() || user_id.len() > MAX_USER_ID {
            return Err(DomError::Type);
        }
        // An empty list means "the client's defaults", which include ES256.
        if !j.pub_key_cred_params.is_empty() && !j.pub_key_cred_params.iter().any(|p| p.ty == "public-key" && p.alg == ES256) {
            return Err(DomError::NotSupported);
        }
        let sel = &j.authenticator_selection;
        let resident_key = match sel.resident_key.as_deref() {
            Some("required") => true,
            Some(_) => false,
            None => sel.require_resident_key,
        };
        Ok(CreationOptions {
            rp_id: j.rp.id,
            rp_name: display(&j.rp.name),
            user_id,
            user_name: display(&j.user.name),
            user_display_name: display(&j.user.display_name),
            challenge: challenge(&j.challenge)?,
            timeout_ms: timeout(j.timeout),
            exclude: descriptors(&j.exclude_credentials)?,
            resident_key,
            uv: uv(sel.user_verification.as_deref()),
            wants_attestation: matches!(j.attestation.as_deref(), Some("direct" | "enterprise" | "indirect")),
        })
    }
}

impl RequestOptions {
    pub fn from_json(json: &str) -> Result<RequestOptions, DomError> {
        if json.len() > 128 * 1024 {
            return Err(DomError::Type);
        }
        let j: RequestJson = serde_json::from_str(json).map_err(|_| DomError::Type)?;
        Ok(RequestOptions {
            rp_id: j.rp_id,
            challenge: challenge(&j.challenge)?,
            timeout_ms: timeout(j.timeout),
            allow: descriptors(&j.allow_credentials)?,
            uv: uv(j.user_verification.as_deref()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATE: &str = r#"{
        "rp": {"id": "example.com", "name": "Example"},
        "user": {"id": "AQIDBA", "name": "alice@example.com", "displayName": "Alice"},
        "challenge": "c2l4dGVlbi1ieXRlcy1sb25n",
        "pubKeyCredParams": [{"type": "public-key", "alg": -8}, {"type": "public-key", "alg": -7}],
        "timeout": 60000,
        "excludeCredentials": [{"type": "public-key", "id": "AAEC"}],
        "authenticatorSelection": {"residentKey": "required", "userVerification": "required"},
        "attestation": "none",
        "extensions": {"credProps": true}
    }"#;

    #[test]
    fn creation_options() {
        let o = CreationOptions::from_json(CREATE).unwrap();
        assert_eq!(o.rp_id.as_deref(), Some("example.com"));
        assert_eq!(o.user_id, vec![1, 2, 3, 4]);
        assert_eq!(o.challenge, b"sixteen-bytes-long");
        assert_eq!(o.timeout_ms, 60_000);
        assert_eq!(o.exclude, vec![vec![0, 1, 2]]);
        assert!(o.resident_key && o.uv == Uv::Required && !o.wants_attestation);
    }

    #[test]
    fn creation_refusals() {
        let no_es256 = CREATE.replace(r#"{"type": "public-key", "alg": -7}"#, r#"{"type": "public-key", "alg": -257}"#);
        assert_eq!(CreationOptions::from_json(&no_es256), Err(DomError::NotSupported));
        assert_eq!(CreationOptions::from_json(&CREATE.replace("AQIDBA", "")), Err(DomError::Type), "empty user id");
        assert_eq!(CreationOptions::from_json(&CREATE.replace("AQIDBA", &"A".repeat(90))), Err(DomError::Type), "user id over 64 bytes");
        assert_eq!(CreationOptions::from_json(&CREATE.replace("c2l4dGVlbi1ieXRlcy1sb25n", "!!")), Err(DomError::Type));
        assert_eq!(CreationOptions::from_json("{}"), Err(DomError::Type));
        let long = CREATE.replace("\"Alice\"", &format!("\"{}\"", "é".repeat(40)));
        assert_eq!(CreationOptions::from_json(&long).unwrap().user_display_name.len(), 64, "cut on a char boundary");
    }

    #[test]
    fn request_options() {
        let o = RequestOptions::from_json(r#"{"challenge":"AQID","allowCredentials":[{"type":"public-key","id":"AAEC"},{"type":"other","id":"!!"}],"userVerification":"discouraged"}"#).unwrap();
        assert_eq!((o.challenge, o.allow, o.uv, o.rp_id, o.timeout_ms), (vec![1, 2, 3], vec![vec![0, 1, 2]], Uv::Discouraged, None, 0));
        let many = format!(r#"{{"challenge":"AQID","allowCredentials":[{}]}}"#, vec![r#"{"type":"public-key","id":"AA"}"#; 65].join(","));
        assert_eq!(RequestOptions::from_json(&many), Err(DomError::Type));
    }
}
