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
    conv.site_check(HOST).map_err(|e| e.to_string())?;
    let Conn::Tls(mut stream) = conv
        .connect(HOST, 443, true, HOST)
        .map_err(|e| e.to_string())?
    else {
        return Err("YouTube entry requires TLS".into());
    };
    if let Some(mime) = asset {
        let path = url
            .strip_prefix(navigator_control::youtube::ORIGIN)
            .ok_or("invalid provider origin")?;
        http::write_request(&mut stream, HOST, path).map_err(|e| e.to_string())?;
        response(&mut *stream, mime, navigator_control::youtube::ASSET_LIMIT)
    } else {
        exchange(&mut *stream)
    }
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
