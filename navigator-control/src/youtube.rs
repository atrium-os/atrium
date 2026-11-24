//! Resources for the opt-in YouTube startup gate. URL input is the serialized
//! URL from LibWeb. The grant covers bootstrap assets, the Play icon and a bounded player POST.
pub const ORIGIN: &str = "https://www.youtube-nocookie.com";
pub const PLAYER: &str = "https://www.youtube-nocookie.com/youtubei/v1/player";
pub const PLAYER_RESPONSE_LIMIT: usize = 512 * 1024;
pub const PLAYER_BODY_LIMIT: usize = 32 * 1024;
pub const PLAYER_REQUESTS: usize = 2;
pub const PLAYER_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

pub fn player_url(url: &str) -> bool {
    url.strip_prefix(PLAYER)
        .is_some_and(|suffix| matches!(suffix, "" | "?prettyPrint=false"))
}

/// Rebuild the anonymous request from the fields needed for the granted video.
/// Only the current embed’s bounded provider fields are sent to the fixed endpoint.
pub fn player_body(input: &[u8]) -> Result<Vec<u8>, &'static str> {
    if input.len() > PLAYER_BODY_LIMIT {
        return Err("player request too large");
    }
    let request: serde_json::Value =
        serde_json::from_slice(input).map_err(|_| "invalid player JSON")?;
    if request["videoId"] != "M7lc1UVf-VE" {
        return Err("outside player video grant");
    }
    let client = &request["context"]["client"];
    let version = client["clientVersion"]
        .as_str()
        .ok_or("missing player version")?;
    if client["clientName"] != "WEB_EMBEDDED_PLAYER"
        || version.is_empty()
        || version.len() > 32
        || !version.bytes().all(|b| b == b'.' || b.is_ascii_digit())
    {
        return Err("invalid player client");
    }
    let mut body = serde_json::json!({
        "videoId": "M7lc1UVf-VE",
        "context": {
            "client":{"clientName":"WEB_EMBEDDED_PLAYER","clientVersion":version,"clientScreen":"EMBED","originalUrl":crate::YOUTUBE_ENTRY},
            "thirdParty":{"embedUrl":"https://org.atrium.navigator/"}
        },
        "playbackContext":{"contentPlaybackContext":{"html5Preference":"HTML5_PREF_WANTS","referer":"https://org.atrium.navigator/"}}
    });
    if let Some(ts) = request
        .pointer("/playbackContext/contentPlaybackContext/signatureTimestamp")
        .and_then(|v| v.as_u64())
        .filter(|&v| v <= u32::MAX as u64)
    {
        body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"] = ts.into();
    }
    // The embed can return its own opaque values only to the same player endpoint.
    for (object, key, max_len) in [
        ("/context/client", "visitorData", 2048),
        ("/serviceIntegrityDimensions", "poToken", 8192),
    ] {
        let Some(value) = request.pointer(object).and_then(|v| v[key].as_str()) else {
            continue;
        };
        if value.len() > max_len
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_+/=.%".contains(&c))
        {
            return Err("invalid player token encoding");
        }
        if object == "/context/client" {
            body["context"]["client"][key] = value.into();
        } else {
            body["serviceIntegrityDimensions"][key] = value.into();
        }
    }
    serde_json::to_vec(&body).map_err(|_| "invalid player payload")
}

pub const PLAY_ICON: &str =
    "https://www.gstatic.com/youtube/img/icons/mweb/youtube_fill/logo/v2/67px.svg";
pub const ICON_LIMIT: usize = 16 * 1024;

pub fn asset_limit(url: &str) -> usize {
    if url == PLAY_ICON {
        ICON_LIMIT
    } else {
        ASSET_LIMIT
    }
}

pub const ASSET_LIMIT: usize = 3 * 1024 * 1024;
pub const ASSET_COUNT: usize = 12;
pub const ASSET_BUDGET: usize = 10 * 1024 * 1024;

pub fn asset_mime(url: &str) -> Option<&'static str> {
    if url == PLAY_ICON {
        return Some("image/svg+xml");
    }
    let path = url.strip_prefix(ORIGIN)?;
    if !path.starts_with('/')
        || path.len() > 2048
        || !path
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.=,".contains(&c))
        || path[1..].split('/').any(|s| matches!(s, "" | "." | ".."))
    {
        return None;
    }
    if path.starts_with("/s/_/ytembeds/_/js/k=") {
        return Some("text/javascript");
    }
    if path.starts_with("/s/_/ytembeds/_/ss/k=") {
        return Some("text/css");
    }
    let (version, resource) = path.strip_prefix("/s/player/")?.split_once('/')?;
    if !version.bytes().all(|c| c.is_ascii_hexdigit()) || version.len() != 8 {
        return None;
    }
    if resource == "www-player.css" {
        return Some("text/css");
    }
    let locale = resource
        .strip_prefix("player_embed.vflset/")?
        .strip_suffix("/base.js")?;
    (locale.len() <= 16
        && !locale.is_empty()
        && locale
            .bytes()
            .all(|c| c.is_ascii_alphabetic() || c == b'_' || c == b'-'))
    .then_some("text/javascript")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_provider_bootstrap_paths() {
        for (path, mime) in [
            (
                "/s/player/1234abcd/player_embed.vflset/en_GB/base.js",
                "text/javascript",
            ),
            ("/s/_/ytembeds/_/ss/k=base/m=root,base", "text/css"),
        ] {
            assert_eq!(asset_mime(&format!("{ORIGIN}{path}")), Some(mime));
        }
        for tail in [
            "",
            ".other/s/player/1234abcd/www-player.css",
            "/s/_/ytembeds/_/js/k=a/../x",
            "/s/_/ytembeds/_/js/k=a?q=b",
            "/s/_/ytembeds/_/js/k=%2f",
            "/youtubei/v1/player",
        ] {
            assert_eq!(asset_mime(&format!("{ORIGIN}{tail}")), None);
        }
    }
}

#[cfg(test)]
#[test]
fn icon_grant_is_one_anonymous_resource() {
    assert_eq!(asset_mime(PLAY_ICON), Some("image/svg+xml"));
    for other in [
        PLAY_ICON.replace("67px", "68px"),
        format!("{PLAY_ICON}?v=1"),
        PLAY_ICON.replace("www.gstatic.com", "other.gstatic.com"),
    ] {
        assert_eq!(asset_mime(&other), None);
    }
}

#[cfg(test)]
#[test]
fn player_grant_checks_url_and_emits_only_granted_fields() {
    assert!(player_url(PLAYER));
    assert!(player_url(&format!("{PLAYER}?prettyPrint=false")));
    for bad in [
        format!("{PLAYER}/next"),
        format!("{PLAYER}?other=1"),
        PLAYER.replace("www.youtube-nocookie.com", "other.example"),
    ] {
        assert!(!player_url(&bad));
    }
    let mut request = serde_json::json!({"videoId":"M7lc1UVf-VE","context":{"client":{"clientName":"WEB_EMBEDDED_PLAYER","clientVersion":"1.2.3","userAgent":"private"}},"extra":"private"});
    let body = player_body(&serde_json::to_vec(&request).unwrap()).unwrap();
    assert!(!String::from_utf8(body).unwrap().contains("private"));
    request["context"]["client"]["visitorData"] = "from_this_embed".into();
    request["serviceIntegrityDimensions"]["poToken"] = "fixture-token".into();
    request["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"] = 12345.into();
    let courier = player_body(&serde_json::to_vec(&request).unwrap()).unwrap();
    assert_eq!(player_body(&courier).unwrap(), courier);
    let parsed: serde_json::Value = serde_json::from_slice(&courier).unwrap();
    assert_eq!(
        parsed["serviceIntegrityDimensions"]["poToken"],
        "fixture-token"
    );
    assert_eq!(
        parsed["context"]["thirdParty"]["embedUrl"],
        "https://org.atrium.navigator/"
    );
    request["serviceIntegrityDimensions"]["poToken"] = "x".repeat(8193).into();
    assert!(player_body(&serde_json::to_vec(&request).unwrap()).is_err());
    request["serviceIntegrityDimensions"]["poToken"] = "fixture-token".into();
    request["videoId"] = "other".into();
    assert!(player_body(&serde_json::to_vec(&request).unwrap()).is_err());
    assert!(player_body(&vec![0; PLAYER_BODY_LIMIT + 1]).is_err());
}
