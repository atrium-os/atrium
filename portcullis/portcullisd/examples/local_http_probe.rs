//! A stand-in web server for scripts/vm-local-server-check.sh
//! (background-apps.md §4.4): on 9000 and 9090 (all addresses) it answers
//! each request with `served <port> <path> cookie=<Cookie or ->`, setting a
//! cookie; `/redirect` redirects to `/hello`, `/away` off this computer,
//! and `/page` answers HTML whose title is that line (so navigatord's
//! `Opened` shows it). `/live` is a page for running live (§4.5): its
//! script GETs `/api/get`, POSTs JSON to `/api/save` (answered with the
//! body it got), tries port 9090 and the web (both must fail), counts its
//! visits in localStorage, and puts what happened in its title, then a
//! link to `/go` (a same-origin link from a live page). Each
//! request's first line goes to stderr.

use std::io::{Read, Write};
use std::net::TcpListener;

fn answer(port: u16, req: &str) -> String {
    let path = req.split(' ').nth(1).unwrap_or("/");
    let cookie = req.lines().find_map(|l| l.strip_prefix("Cookie: ").or_else(|| l.strip_prefix("cookie: "))).unwrap_or("-");
    match path {
        "/live" => {
            let body = r#"<!doctype html><html><head><title>live starting</title></head><body><p>live starting</p><script>
(async () => {
  const r = [];
  try { r.push('get=' + await (await fetch('/api/get')).text()); } catch (e) { r.push('get=ERR'); }
  try { r.push('post=' + await (await fetch('/api/save', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"n":1}' })).text()); } catch (e) { r.push('post=ERR'); }
  try { await fetch('http://localhost:9090/x'); r.push('other-port=REACHED'); } catch (e) { r.push('other-port=refused'); }
  try { await fetch('https://example.com/'); r.push('web=REACHED'); } catch (e) { r.push('web=refused'); }
  const n = (+localStorage.getItem('visits') || 0) + 1;
  localStorage.setItem('visits', String(n));
  r.push('visits=' + n);
  document.title = 'live ' + r.join(' ');
  document.body.textContent = document.title;
  const a = document.createElement('a');
  a.href = '/go'; a.id = 'go'; a.textContent = 'next page';
  document.body.append(' ', a);
})();
</script></body></html>"#;
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
        "/api/get" => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
        "/api/save" => {
            let got = req.split_once("\r\n\r\n").map_or("", |(_, b)| b).replace(' ', "");
            let body = format!("saved{got}");
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
        "/redirect" => "HTTP/1.1 302 Found\r\nLocation: /hello\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "/away" => "HTTP/1.1 302 Found\r\nLocation: http://example.com/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "/page" | "/go" => {
            let line = format!("served {port} {path} cookie={}", cookie.trim());
            let body = format!("<!doctype html><html><head><title>{line}</title></head><body><p>{line}</p></body></html>");
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nSet-Cookie: s={port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
        _ => {
            let body = format!("served {port} {path} cookie={}", cookie.trim());
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: s={port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }
    }
}

fn main() {
    let mut threads = Vec::new();
    for port in [9000u16, 9090] {
        threads.push(std::thread::spawn(move || {
            let l = match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("local_http_probe: bind {port}: {e}");
                    std::process::exit(1);
                }
            };
            for mut c in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    let mut got = 0;
                    while got < buf.len() {
                        match c.read(&mut buf[got..]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                        if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    // A POST's body: read up to its declared length.
                    if let Some(at) = buf[..got].windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let want = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                        while got < buf.len() && got < at + 4 + want {
                            match c.read(&mut buf[got..]) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => got += n,
                            }
                        }
                    }
                    let req = String::from_utf8_lossy(&buf[..got]).to_string();
                    eprintln!("local_http_probe: {port} {}", req.lines().next().unwrap_or(""));
                    let _ = c.write_all(answer(port, &req).as_bytes());
                });
            }
        }));
    }
    for t in threads {
        let _ = t.join();
    }
}
