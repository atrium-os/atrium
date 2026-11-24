//! The current embed may use the delivery URL issued by its successful player reply.
//! Preserve the signed query. The provider script fills in its transport counters.
use std::collections::BTreeMap;

pub const MIME: &str = "application/vnd.yt-ump";
pub const BODY: usize = 8192;
pub const REPLY: usize = 2 * 1024 * 1024;
pub const REQUESTS: usize = 12;

pub fn endpoint(url: &str) -> Option<(&str, &str)> {
    if url.len() > 8192
        || !url
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'#' | b'\\'))
    {
        return None;
    }
    let (host, query) = url
        .strip_prefix("https://")?
        .split_once("/videoplayback?")?;
    let label = host.strip_suffix(".googlevideo.com")?;
    if label.is_empty()
        || label.len() > 63
        || !label
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || query.is_empty()
    {
        return None;
    }
    Some((host, query))
}

fn fields(query: &str) -> Option<BTreeMap<&str, &str>> {
    let mut map = BTreeMap::new();
    for field in query.split('&') {
        let (k, v) = field.split_once('=')?;
        if k.is_empty()
            || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || map.len() == 128
            || map.insert(k, v).is_some()
        {
            return None;
        }
    }
    Some(map)
}

pub fn issued(body: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    if json["playabilityStatus"]["status"] != "OK" {
        return None;
    }
    let url = json["streamingData"]["serverAbrStreamingUrl"].as_str()?;
    fields(endpoint(url)?.1)?;
    Some(url.to_owned())
}

pub fn permits(grant: &str, target: &str) -> bool {
    fn matches(grant: &str, target: &str) -> Option<bool> {
        let (host, query) = endpoint(grant)?;
        let (next_host, next_query) = endpoint(target)?;
        if host != next_host {
            return Some(false);
        }
        let base = fields(query)?;
        let mut next = fields(next_query)?;
        let token = |v: &str| {
            !v.is_empty()
                && v.len() <= 256
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        };
        for (k, v) in base {
            let actual = next.remove(k)?;
            if actual != v && !(k == "n" && token(actual)) {
                return Some(false);
            }
        }
        Some(next.into_iter().all(|(k, v)| match k {
            "rn" => v.len() <= 6 && !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()),
            "cpn" => v.len() == 16 && token(v),
            "cver" => {
                v.len() <= 32 && !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit() || c == b'.')
            }
            "alr" => matches!(v, "yes" | "no"),
            _ => false,
        }))
    }
    matches(grant, target) == Some(true)
}

#[test]
fn delivery_is_bound_to_the_authenticated_reply() {
    let grant = "https://rr3.googlevideo.com/videoplayback?id=one&n=old";
    let valid = "https://rr3.googlevideo.com/videoplayback?n=new&id=one&rn=2";
    assert!(permits(grant, valid));
    for bad in [
        valid.replace("rr3", "rr4"),
        valid.replace("id=one", "id=two"),
        format!("{valid}&rn=3"),
        format!("{valid}&x=y"),
        valid.replace("rr3", "user@rr3"),
        valid.replace("rr3", "127.0.0.1/rr3"),
        valid.replace("id=one&", ""),
    ] {
        assert!(!permits(grant, &bad));
    }
}
