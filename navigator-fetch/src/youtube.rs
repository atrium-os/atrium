//! Bounded HTTPS GETs for the installed I4 embed. Only its fixed entry sends
//! the app ID. Its versioned bootstrap resources use anonymous requests.
use crate::{http, Conn, Conversation};
use std::io::{Read, Write};

pub const HOST: &str = "www.youtube-nocookie.com";
pub const ENTRY_LIMIT: usize = 512 * 1024;
const REQUEST: &[u8] = b"GET /embed/M7lc1UVf-VE HTTP/1.1\r\nHost: www.youtube-nocookie.com\r\nReferer: https://org.atrium.navigator/\r\nUser-Agent: atrium-navigator-fetch/0.1\r\nAccept: text/html\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n";

pub fn resource(url: &str, conv: &mut Conversation<'_>) -> Result<Vec<u8>, String> {
    let asset = if url == navigator_control::YOUTUBE_ENTRY {
        None
    } else {
        Some(navigator_control::youtube::asset_mime(url).ok_or("outside startup grant")?)
    };
    let icon = url == navigator_control::youtube::PLAY_ICON;
    let host = if icon { "www.gstatic.com" } else { HOST };
    conv.site_check(host).map_err(|e| e.to_string())?;
    let Conn::Tls(mut stream) = conv
        .connect(host, 443, true, host)
        .map_err(|e| e.to_string())?
    else {
        return Err("YouTube entry requires TLS".into());
    };
    if let Some(mime) = asset {
        if icon {
            let path = url
                .strip_prefix("https://www.gstatic.com")
                .ok_or("invalid icon origin")?;
            write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: {}\r\nAccept: image/svg+xml\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", navigator_control::youtube::ORIGIN).and_then(|_| stream.flush()).map_err(|e| e.to_string())?;
            return response(&mut *stream, mime, navigator_control::youtube::ICON_LIMIT);
        }
        let path = url
            .strip_prefix(navigator_control::youtube::ORIGIN)
            .ok_or("invalid provider origin")?;
        http::write_request(&mut stream, HOST, path).map_err(|e| e.to_string())?;
        response(&mut *stream, mime, navigator_control::youtube::ASSET_LIMIT)
    } else {
        exchange(&mut *stream)
    }
}

pub fn player(input: &[u8], conv: &mut Conversation<'_>) -> Result<Vec<u8>, String> {
    use navigator_control::youtube::player_body;
    let body = player_body(input)?;
    conv.site_check(HOST).map_err(|e| e.to_string())?;
    let Conn::Tls(mut stream) = conv
        .connect(HOST, 443, true, HOST)
        .map_err(|e| e.to_string())?
    else {
        return Err("player requires TLS".into());
    };
    exchange_player(&mut *stream, &body)
}

fn exchange_player(stream: &mut (impl Read + Write), body: &[u8]) -> Result<Vec<u8>, String> {
    use navigator_control::youtube::{ORIGIN, PLAYER_RESPONSE_LIMIT};
    write!(stream, "POST /youtubei/v1/player?prettyPrint=false HTTP/1.1\r\nHost: {HOST}\r\nOrigin: {ORIGIN}\r\nReferer: {ORIGIN}/embed/M7lc1UVf-VE\r\nContent-Type: application/json\r\nAccept: application/json\r\nAccept-Encoding: identity\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).and_then(|_|stream.write_all(body)).and_then(|_|stream.flush()).map_err(|e|e.to_string())?;
    response(stream, "application/json", PLAYER_RESPONSE_LIMIT)
}

fn exchange(stream: &mut (impl Read + Write)) -> Result<Vec<u8>, String> {
    stream
        .write_all(REQUEST)
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())?;
    response(stream, "text/html", ENTRY_LIMIT)
}
fn response(stream: &mut impl Read, expected: &str, limit: usize) -> Result<Vec<u8>, String> {
    let (response, _) = http::read_response_capped(stream, Some(limit + 1))
        .map_err(|e| format!("YouTube entry: {e:?}"))?;
    if response.status != 200 {
        return Err(format!("YouTube entry returned HTTP {}", response.status));
    }
    if expected == "image/svg+xml"
        && !matches!(
            response
                .header("access-control-allow-origin")
                .map(str::trim),
            Some("*" | navigator_control::youtube::ORIGIN)
        )
    {
        return Err("Play icon did not grant access to the embed origin".into());
    }
    let mime = response
        .header("content-type")
        .and_then(|v| v.split(';').next());
    if !mime.is_some_and(|v| {
        v.trim().eq_ignore_ascii_case(expected)
            || (expected == "text/javascript"
                && v.trim().eq_ignore_ascii_case("application/javascript"))
    }) {
        return Err(format!(
            "YouTube resource requires {expected}, received {mime:?}"
        ));
    }
    if response.body.is_empty() || response.body.len() > limit {
        return Err("invalid YouTube entry size".into());
    }
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    struct Wire {
        reply: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }
    impl Read for Wire {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.reply.read(out)
        }
    }
    impl Write for Wire {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.sent.write(input)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn player_post_uses_fixed_headers_and_refuses_redirects() {
        let mut wire = Wire {
            sent: Vec::new(),
            reply: Cursor::new(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
                    .to_vec(),
            ),
        };
        assert_eq!(exchange_player(&mut wire, b"{}").unwrap(), b"{}");
        let request = std::str::from_utf8(&wire.sent).unwrap();
        assert!(request.starts_with("POST /youtubei/v1/player?prettyPrint=false HTTP/1.1\r\nHost: www.youtube-nocookie.com\r\n"));
        assert!(request.contains("Content-Length: 2\r\n"));
        assert!(request.ends_with("\r\n\r\n{}"));
        assert!(!request.to_ascii_lowercase().contains("cookie:"));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        wire.reply = Cursor::new(
            b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /other\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        );
        assert!(exchange_player(&mut wire, b"{}")
            .unwrap_err()
            .contains("307"));
    }

    #[test]
    fn icon_requires_cross_origin_permission_and_svg_mime() {
        for (origin, mime, ok) in [
            ("", "image/svg+xml", false),
            ("https://other.example", "image/svg+xml", false),
            ("*", "text/html", false),
            (navigator_control::youtube::ORIGIN, "image/svg+xml", true),
            ("*", "image/svg+xml", true),
        ] {
            let wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nAccess-Control-Allow-Origin: {origin}\r\nContent-Length: 3\r\n\r\nsvg"
            );
            assert_eq!(
                response(&mut wire.as_bytes(), "image/svg+xml", 3).is_ok(),
                ok
            );
        }
    }

    #[test]
    fn entry_only_sends_the_package_identity_and_refuses_redirects() {
        let mut entry = Wire {
            reply: Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: 3\r\n\r\nabc".to_vec()),
            sent: Vec::new(),
        };
        assert_eq!(exchange(&mut entry).unwrap(), b"abc");
        let request = std::str::from_utf8(&entry.sent).unwrap();
        assert_eq!(
            request.lines().next(),
            Some("GET /embed/M7lc1UVf-VE HTTP/1.1")
        );
        assert_eq!(
            request
                .lines()
                .filter(|l| l.starts_with("Referer:"))
                .collect::<Vec<_>>(),
            ["Referer: https://org.atrium.navigator/"]
        );
        let mut ordinary = Vec::new();
        http::write_request(&mut ordinary, HOST, "/embed/M7lc1UVf-VE").unwrap();
        assert!(!String::from_utf8(ordinary)
            .unwrap()
            .to_lowercase()
            .contains("referer:"));

        entry.reply = Cursor::new(
            b"HTTP/1.1 302 Found\r\nLocation: /another\r\nContent-Length: 0\r\n\r\n".to_vec(),
        );
        entry.sent.clear();
        assert!(exchange(&mut entry).unwrap_err().contains("302"));
        assert_eq!(entry.sent, REQUEST);
    }
}
