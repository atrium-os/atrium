//! Resources for the opt-in YouTube startup gate. URL input is the serialized
//! URL from LibWeb. The grant includes versioned scripts and styles and one anonymous Play icon.
pub const ORIGIN: &str = "https://www.youtube-nocookie.com";
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
