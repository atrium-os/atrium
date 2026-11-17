//! opifexd's network (opifex.md §7.2 step 2, build step C4).
//!
//! opifexd has no network. When it needs bytes from the internet (a sync, an
//! archive), it starts `opifex-fetch`, an ordinary launched system bundle
//! (`org.atrium.opifex.fetch`) whose only capability is `net-fetch` narrowed
//! by `net-fetch-sites`, so every byte comes through Nuntius in the
//! fetcher's jail. opifexd starts it with `portcullis launch`, whose stdio
//! are handed to the jailed app: opifexd's pipes are the fetcher's stdin and
//! stdout, private to the two of them. Nothing the fetcher returns is
//! trusted: catalog files are checked by signature and tree, archives by
//! hash and size.
//!
//! The wire: opifexd writes one JSON [`Ask`] per line; the fetcher answers
//! each with one JSON [`Got`] line, followed by exactly `len` body bytes when
//! it is `Ok`. EOF on its stdin ends the fetcher.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

/// The fetcher's bundle id.
pub const FETCH_APP: &str = "org.atrium.opifex.fetch";
/// How opifexd starts it unless told otherwise (`OPIFEX_FETCH`, space separated).
pub const FETCH_CMD: &[&str] = &["/usr/local/bin/portcullis", "launch", "--no-prompt", FETCH_APP];
/// Where the Atrium catalog is served until a signed `locations` entry says
/// otherwise (§6.4).
pub const CATALOG_URL: &str = "https://atrium-os.github.io/catalog/";
/// The largest catalog file (a head, a bundle of 256 entries).
pub const CATALOG_FILE_MAX: u64 = 16 << 20;
pub const URL_MAX: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ask {
    /// GET `url` (https only), at most `max` bytes.
    Get { url: String, max: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Got {
    /// `len` bytes follow.
    Ok { len: u64 },
    /// Not fetched (an HTTP status, a refusal, too big).
    Err { why: String },
}

/// Whether `url` may be asked for at all: https, no credentials, bounded.
pub fn plain_https(url: &str) -> bool {
    url.len() <= URL_MAX
        && url.strip_prefix("https://").is_some_and(|rest| {
            let host = rest.split('/').next().unwrap_or("");
            !host.is_empty() && !host.contains('@') && rest.bytes().all(|b| b.is_ascii_graphic())
        })
}

/// `base` joined with a catalog `path` (`head`, `entries/000000`).
pub fn join(base: &str, path: &str) -> String {
    if base.ends_with('/') { format!("{base}{path}") } else { format!("{base}/{path}") }
}

/// opifexd's end: a running fetcher, started on first use.
pub struct Fetcher {
    cmd: Vec<String>,
    run: Option<(Child, ChildStdin, BufReader<ChildStdout>)>,
}

impl Fetcher {
    pub fn new(cmd: Vec<String>) -> Fetcher {
        Fetcher { cmd, run: None }
    }

    fn start(&mut self) -> Result<(), String> {
        if self.run.is_some() {
            return Ok(());
        }
        let (prog, args) = self.cmd.split_first().ok_or("no fetcher configured")?;
        let mut c = Command::new(prog).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn().map_err(|e| format!("starting the fetcher ({prog}): {e}"))?;
        let (i, o) = (c.stdin.take().ok_or("fetcher stdin")?, c.stdout.take().ok_or("fetcher stdout")?);
        self.run = Some((c, i, BufReader::new(o)));
        Ok(())
    }

    /// The body of `url`, at most `max` bytes.
    pub fn get(&mut self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        if !plain_https(url) {
            return Err(format!("{url}: not a plain https URL"));
        }
        self.start()?;
        let r = (|| {
            let (_, i, o) = self.run.as_mut().expect("started");
            let mut line = serde_json::to_vec(&Ask::Get { url: url.into(), max }).map_err(|e| e.to_string())?;
            line.push(b'\n');
            i.write_all(&line).and_then(|_| i.flush()).map_err(|e| format!("fetcher: {e}"))?;
            let mut back = String::new();
            o.by_ref().take(4096).read_line(&mut back).map_err(|e| format!("fetcher: {e}"))?;
            if back.is_empty() {
                return Err("the fetcher ended (is org.atrium.opifex.fetch installed and granted?)".to_string());
            }
            match serde_json::from_str::<Got>(back.trim_end()).map_err(|e| format!("fetcher: {e}"))? {
                Got::Err { why } => Err(format!("{url}: {why}")),
                Got::Ok { len } if len > max => Err(format!("{url}: bigger than {max} bytes")),
                Got::Ok { len } => {
                    let mut body = Vec::with_capacity(len as usize);
                    o.by_ref().take(len).read_to_end(&mut body).map_err(|e| format!("fetcher: {e}"))?;
                    if body.len() as u64 != len {
                        return Err(format!("{url}: the fetcher sent {} of {len} bytes", body.len()));
                    }
                    Ok(body)
                }
            }
        })();
        // After a broken exchange the stream can't be trusted to be in step.
        if r.as_ref().is_err_and(|e| e.starts_with("fetcher:") || e.contains("the fetcher")) {
            self.stop();
        }
        r
    }

    pub fn stop(&mut self) {
        if let Some((mut c, i, _)) = self.run.take() {
            drop(i);
            let _ = c.wait();
        }
    }
}

impl Drop for Fetcher {
    fn drop(&mut self) { self.stop() }
}

/// The fetcher's end: answer each [`Ask`] on `input` with `get`, until EOF.
pub fn serve(input: impl Read, mut out: impl Write, mut get: impl FnMut(&str, u64) -> Result<Vec<u8>, String>) -> io::Result<()> {
    let mut input = BufReader::new(input);
    loop {
        let mut line = String::new();
        if input.by_ref().take(URL_MAX as u64 + 256).read_line(&mut line)? == 0 {
            return Ok(());
        }
        let got = match serde_json::from_str::<Ask>(line.trim_end()) {
            Ok(Ask::Get { url, max }) if plain_https(&url) => get(&url, max).and_then(|b| {
                if b.len() as u64 > max { Err(format!("bigger than {max} bytes")) } else { Ok(b) }
            }),
            Ok(Ask::Get { .. }) => Err("not a plain https URL".into()),
            Err(e) => Err(format!("bad ask: {e}")),
        };
        let (head, body) = match got {
            Ok(b) => (Got::Ok { len: b.len() as u64 }, b),
            Err(why) => (Got::Err { why }, Vec::new()),
        };
        let mut l = serde_json::to_vec(&head).map_err(io::Error::other)?;
        l.push(b'\n');
        out.write_all(&l)?;
        out.write_all(&body)?;
        out.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(plain_https("https://atrium-os.github.io/catalog/head"));
        for u in ["http://x/", "https://", "https://u:p@x/", "https://x/a b", "file:///etc/passwd"] {
            assert!(!plain_https(u), "{u}");
        }
        assert_eq!(join("https://a/c/", "head"), "https://a/c/head");
        assert_eq!(join("https://a/c", "head"), "https://a/c/head");
    }

    #[test]
    fn serve_answers_in_order() {
        let asks = [Ask::Get { url: "https://a/x".into(), max: 10 }, Ask::Get { url: "https://a/big".into(), max: 2 },
                    Ask::Get { url: "http://a/x".into(), max: 10 }];
        let input: Vec<u8> = asks.iter().flat_map(|a| { let mut v = serde_json::to_vec(a).unwrap(); v.push(b'\n'); v }).collect();
        let mut out = Vec::new();
        serve(&input[..], &mut out, |u, _| Ok(u.as_bytes()[8..].to_vec())).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "{\"ok\":{\"len\":3}}\na/x{\"err\":{\"why\":\"bigger than 2 bytes\"}}\n{\"err\":{\"why\":\"not a plain https URL\"}}\n");
    }
}
