//! The jailed converter's protocol (threat model §2.2, backend §4.7a).
//!
//! ★★ THE PROCESS THAT RUNS THE PAGE'S JAVASCRIPT HOLDS NOTHING. It is a
//! one-shot jail entry with zero capabilities: no network, no host
//! filesystem, no arguments and no environment worth reading (a jail launches
//! its entry bare). Everything it needs arrives on stdin and everything it
//! produces leaves on stdout:
//!
//! ```text
//! parent → child   atrium-prerender/1 \t key=value … \t len=<n> \n  <n document bytes>
//!                  then one fetchd reply frame per `F` request
//! child → parent   F <url> \n                      a fetch, relayed by the parent
//!                  <TAG> \t …  \n                  the report (R, L, O, … as `--one` prints)
//!                  BLOB <blake3:hex> <n> \n <n>    an image, by content address
//!                  RECORDING <n> \n <n>            the publishable recording
//!                  TIMEOUT \n                      the child's own deadline fired
//!                  END \n
//! ```
//!
//! ★ WHY THE FETCHER IS RELAYED, NOT INHERITED. `portcullis exec --daemon`
//! hands the jail exactly three descriptors over SCM_RIGHTS (portcullis.md
//! §6.5.2a), so a fourth descriptor to fetchd would not survive the trip.
//! The converter's stdin/stdout ARE its inherited descriptors, and the
//! unconfined parent forwards each `F` line to the conversion's own jailed
//! fetchd and its reply frame back, verbatim. The parent holds no more than
//! it held before (it already started fetchd); the child holds less.
//!
//! ★ EVERYTHING THE CHILD SAYS IS UNTRUSTED, because it is jailed precisely
//! because it may be compromised. Every length is bounded before a byte is
//! allocated, a blob whose bytes do not hash to its claimed address is a
//! lie (the conversion fails rather than storing it), and a line that is
//! not one of the protocol's own is refused — that is also what catches a
//! launcher printing on stdout (backend §4.7a bug 1).

use std::io::{BufRead, Read, Write};
use std::sync::Mutex;

pub const MAGIC: &str = "atrium-prerender/1";
pub const MAX_HEADER: usize = 64 * 1024;
pub const MAX_DOC: usize = 64 * 1024 * 1024;
pub const MAX_BLOB: usize = 8 * 1024 * 1024;
pub const MAX_RECORDING: usize = 128 * 1024 * 1024;
pub const MAX_REPORT: usize = 8 * 1024 * 1024;
pub const MAX_URL: usize = 16 * 1024;
/// One report line. ★ Report lines are not URLs: an error message or a
/// coverage line naming many effect targets runs past 16 KiB on real pages
/// (tripadvisor, walgreens, old.reddit in the corpus), and bounding every line
/// at the URL limit failed those conversions as protocol errors.
pub const MAX_LINE: usize = 1024 * 1024;

/// One conversion, as the parent describes it to the child.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub base: Option<String>,
    pub net: bool,
    pub explore: bool,
    pub same_site: bool,
    pub protect: bool,
    /// Send the recording back.
    pub emit: bool,
    /// Send image bytes back (`BLOB` frames).
    pub blobs: bool,
    /// The child must find itself in a jail, or refuse to convert.
    pub require_confined: bool,
    /// The child's own deadline, in seconds; also its RLIMIT_CPU.
    pub deadline_s: u64,
    /// The child's address-space ceiling, in MiB (FreeBSD; RLIMIT_AS).
    pub mem_mb: u64,
    pub len: usize,
}

impl Default for Job {
    fn default() -> Self {
        Job { base: None, net: false, explore: false, same_site: false, protect: false, emit: false,
              blobs: false, require_confined: false, deadline_s: 30, mem_mb: 4096, len: 0 }
    }
}

fn b(v: bool) -> &'static str { if v { "1" } else { "0" } }

impl Job {
    pub fn header(&self) -> Result<String, String> {
        let base = self.base.as_deref().unwrap_or("");
        if base.contains(['\t', '\n', '\r']) { return Err(format!("base URL with a control character: {base:?}")) }
        Ok(format!("{MAGIC}\tbase={base}\tnet={}\texplore={}\tsame_site={}\tprotect={}\temit={}\tblobs={}\
                    \trequire_confined={}\tdeadline={}\tmem_mb={}\tlen={}\n",
            b(self.net), b(self.explore), b(self.same_site), b(self.protect), b(self.emit), b(self.blobs),
            b(self.require_confined), self.deadline_s, self.mem_mb, self.len))
    }

    /// ★ Strict: an unknown key, a missing `len`, or a value that does not
    /// parse is a refusal. A header the child half-understood would run a
    /// conversion its parent did not ask for.
    pub fn parse(line: &str) -> Result<Job, String> {
        let mut it = line.split('\t');
        if it.next() != Some(MAGIC) { return Err(format!("not a {MAGIC} header")) }
        let mut j = Job::default();
        let mut have_len = false;
        let flag = |v: &str| match v { "0" => Ok(false), "1" => Ok(true), _ => Err(format!("bad flag {v:?}")) };
        for kv in it {
            let (k, v) = kv.split_once('=').ok_or_else(|| format!("bad field {kv:?}"))?;
            match k {
                "base" => j.base = (!v.is_empty()).then(|| v.to_string()),
                "net" => j.net = flag(v)?,
                "explore" => j.explore = flag(v)?,
                "same_site" => j.same_site = flag(v)?,
                "protect" => j.protect = flag(v)?,
                "emit" => j.emit = flag(v)?,
                "blobs" => j.blobs = flag(v)?,
                "require_confined" => j.require_confined = flag(v)?,
                "deadline" => j.deadline_s = v.parse().map_err(|_| format!("bad deadline {v:?}"))?,
                "mem_mb" => j.mem_mb = v.parse().map_err(|_| format!("bad mem_mb {v:?}"))?,
                "len" => { j.len = v.parse().map_err(|_| format!("bad len {v:?}"))?; have_len = true }
                _ => return Err(format!("unknown field {k:?}")),
            }
        }
        if !have_len { return Err("header without len".into()) }
        if j.len > MAX_DOC { return Err(format!("a {}-byte document is over the {MAX_DOC} limit", j.len)) }
        Ok(j)
    }
}

/// Read the header and the document from the child's stdin.
pub fn read_job(r: &mut impl BufRead) -> Result<(Job, Vec<u8>), String> {
    let line = crate::fetch::read_line_bounded(r, MAX_HEADER)?.ok_or("no header: stdin closed")?;
    let job = Job::parse(&line)?;
    let mut doc = vec![0u8; job.len];
    r.read_exact(&mut doc).map_err(|e| format!("short document: {e}"))?;
    Ok((job, doc))
}

/// What the parent got back from one conversion.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The report lines, as `--one` prints them; `parse_child` reads these.
    pub report: String,
    pub recording: Option<Vec<u8>>,
    pub blobs: usize,
    pub fetches: usize,
    pub timed_out: bool,
    pub ended: bool,
}

/// A report line: a short upper-case tag, a tab. Nothing else is.
fn is_report_line(l: &str) -> bool {
    let tag = l.split('\t').next().unwrap_or("");
    l.contains('\t') && (1..=2).contains(&tag.len())
        && tag.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && tag.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn read_n(r: &mut impl Read, n: usize, what: &str) -> Result<Vec<u8>, String> {
    let mut v = vec![0u8; n];
    r.read_exact(&mut v).map_err(|e| format!("short {what}: {e}"))?;
    Ok(v)
}

fn len_of(s: &str, max: usize, what: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("bad {what} length {s:?}"))?;
    if n > max { return Err(format!("child claims a {n}-byte {what}, over the {max} limit")) }
    Ok(n)
}

/// The parent's side: read everything the child says until `END`, relaying
/// each fetch through `fetch` (which returns a fetchd reply frame) and
/// handing each verified blob to `blob`.
///
/// `stdin` is shared so the deadline watcher can CLOSE it — end-of-input is
/// how a jailed child (and its launcher) is asked to exit, and a kill is
/// what leaks the jail (backend §4.7a bug 2).
pub fn drive(
    out: &mut impl BufRead,
    stdin: &Mutex<Option<Box<dyn Write + Send>>>,
    fetch: &mut dyn FnMut(&str) -> Vec<u8>,
    blob: &mut dyn FnMut(&str, &[u8]),
) -> Result<Outcome, String> {
    let mut o = Outcome::default();
    // Set once the child stops reading: it has exited (its own watchdog, a
    // crash) or closed its input. Not a protocol error — what it says next on
    // stdout (TIMEOUT, or nothing) decides how the conversion is reported.
    let mut child_gone = false;
    loop {
        let Some(line) = crate::fetch::read_line_bounded(out, MAX_LINE)? else { break };
        if let Some(url) = line.strip_prefix("F ") {
            o.fetches += 1;
            if child_gone { continue }
            // ★ An oversized URL fails THAT FETCH, not the conversion. Real pages
            // ask for them (19 KB to 179 KB in the corpus: tripadvisor,
            // walgreens, old.reddit), and the old in-process path simply let
            // the fetch fail; failing the whole document was this relay's bug.
            let frame = if url.len() > MAX_URL {
                crate::fetch::Reply::Err(format!("refused: a {}-byte URL, over the {MAX_URL}-byte limit", url.len())).to_frame()
            } else {
                fetch(url)
            };
            let mut g = stdin.lock().map_err(|_| "stdin lock poisoned")?;
            let Some(w) = g.as_mut() else { return Err("fetch after the child's input was closed".into()) };
            if w.write_all(&frame).and_then(|_| w.flush()).is_err() { child_gone = true }
        } else if let Some(rest) = line.strip_prefix("BLOB ") {
            let (addr, n) = rest.split_once(' ').ok_or("malformed BLOB line")?;
            let n = len_of(n, MAX_BLOB, "blob")?;
            let bytes = read_n(out, n, "blob")?;
            // ★ The address is checked, not believed: a compromised child
            // naming bytes by someone else's hash would poison the store
            // (threat model §2.6) under a name every reader trusts.
            let want = format!("blake3:{}", blake3::hash(&bytes).to_hex());
            if addr != want { return Err(format!("blob claims {addr} but hashes to {want}")) }
            blob(addr, &bytes);
            o.blobs += 1;
        } else if let Some(n) = line.strip_prefix("RECORDING ") {
            if o.recording.is_some() { return Err("a second RECORDING".into()) }
            let n = len_of(n, MAX_RECORDING, "recording")?;
            o.recording = Some(read_n(out, n, "recording")?);
        } else if line == "TIMEOUT" {
            o.timed_out = true;
        } else if line == "END" {
            o.ended = true;
            break;
        } else if is_report_line(&line) {
            if o.report.len() + line.len() > MAX_REPORT { return Err("report over its limit".into()) }
            o.report.push_str(&line);
            o.report.push('\n');
        } else {
            // ★ Not skipped. `jail(8)` printing `<name>: created` on stdout is
            // exactly how the worker protocol was corrupted once (§4.7a).
            return Err(format!("unexpected line on the converter's stdout (launcher chatter?): {:?}",
                               line.chars().take(120).collect::<String>()));
        }
    }
    Ok(o)
}

/// Whether the kernel says this process is in a jail. ★ Asked of the kernel,
/// not inferred from how we were started: a require-confinement switch that
/// trusted its own launch path would pass whenever the launcher silently ran
/// the entry bare.
pub fn kernel_says_jailed() -> Result<bool, String> {
    #[cfg(target_os = "freebsd")]
    {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let name = std::ffi::CString::new("security.jail.jailed").unwrap();
        let rc = unsafe { libc::sysctlbyname(name.as_ptr(), &mut v as *mut _ as *mut libc::c_void, &mut len, std::ptr::null(), 0) };
        if rc != 0 { return Err(format!("sysctl security.jail.jailed: {}", std::io::Error::last_os_error())) }
        Ok(v == 1)
    }
    #[cfg(not(target_os = "freebsd"))]
    { Ok(false) }
}

/// Lower this process's CPU-time and address-space ceilings, and read them
/// back. Interim for threat model §2.2 P2 until the jail lane carries rctl
/// limits: a HARD rlimit an unprivileged process lowers cannot be raised
/// again, so a compromised converter inherits the ceiling it was given.
///
/// ★ VERIFIED, not assumed: the limit is read back with getrlimit, and a
/// mismatch is an error (a privilege drop must verify itself).
pub fn limit_self(cpu_s: u64, mem_mb: u64) -> Result<String, String> {
    #[cfg(unix)]
    {
        let set = |res, ask: u64, what: &str| -> Result<(), String> {
            // ★ THE TIGHTER of the ask and what this process inherited. A
            // broker already running under a lower hard limit made the child
            // try to RAISE it — EPERM, and a refused conversion (found in the
            // FreeBSD jail run). Lowering is all this is for; a stricter
            // inherited ceiling is already the goal.
            let mut cur = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if unsafe { libc::getrlimit(res, &mut cur) } != 0 {
                return Err(format!("getrlimit {what}: {}", std::io::Error::last_os_error()));
            }
            let n = if cur.rlim_max == libc::RLIM_INFINITY { ask } else { ask.min(cur.rlim_max as u64) };
            let lim = libc::rlimit { rlim_cur: n as libc::rlim_t, rlim_max: n as libc::rlim_t };
            if unsafe { libc::setrlimit(res, &lim) } != 0 {
                return Err(format!("setrlimit {what}={n}: {}", std::io::Error::last_os_error()));
            }
            let mut got = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if unsafe { libc::getrlimit(res, &mut got) } != 0 || got.rlim_max as u64 != n || got.rlim_cur as u64 != n {
                return Err(format!("{what}: asked for {n}, kernel reports {}/{}", got.rlim_cur, got.rlim_max));
            }
            Ok(())
        };
        set(libc::RLIMIT_CPU, cpu_s, "RLIMIT_CPU")?;
        // macOS refuses RLIMIT_AS below its own reservations; the ceiling
        // that matters is the FreeBSD one, where the jail runs.
        #[cfg(target_os = "freebsd")]
        set(libc::RLIMIT_AS, mem_mb * 1024 * 1024, "RLIMIT_AS")?;
        #[cfg(target_os = "freebsd")]
        return Ok(format!("cpu {cpu_s}s, address space {mem_mb} MiB"));
        #[cfg(not(target_os = "freebsd"))]
        return Ok(format!("cpu {cpu_s}s (address space not capped on this OS; asked {mem_mb} MiB)"));
    }
    #[cfg(not(unix))]
    { let _ = (cpu_s, mem_mb); Ok("no limits on this OS".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdin_sink() -> (std::sync::Arc<Mutex<Vec<u8>>>, Mutex<Option<Box<dyn Write + Send>>>) {
        struct W(std::sync::Arc<Mutex<Vec<u8>>>);
        impl Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(b); Ok(b.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let buf = std::sync::Arc::new(Mutex::new(vec![]));
        (buf.clone(), Mutex::new(Some(Box::new(W(buf)))))
    }

    fn run(child_out: &[u8]) -> (Result<Outcome, String>, Vec<u8>, Vec<String>, Vec<String>) {
        let (buf, stdin) = stdin_sink();
        let mut urls = vec![];
        let mut blobs = vec![];
        let r = drive(&mut &child_out[..], &stdin,
            &mut |u| { urls.push(u.to_string()); b"ok 200 0 2 https://a.example/\nhi".to_vec() },
            &mut |a, _| blobs.push(a.to_string()));
        let sent = buf.lock().unwrap().clone();
        (r, sent, urls, blobs)
    }

    #[test]
    fn a_header_round_trips_and_is_strict() {
        let j = Job { base: Some("https://a.example/x?y".into()), net: true, explore: true, emit: true,
                      require_confined: true, deadline_s: 180, len: 12, ..Job::default() };
        let h = j.header().unwrap();
        assert_eq!(Job::parse(h.trim_end()).unwrap(), j);
        assert!(Job::parse("atrium-prerender/1\tnet=1").unwrap_err().contains("len"));
        assert!(Job::parse("atrium-prerender/1\tlen=1\tsurprise=1").unwrap_err().contains("unknown"));
        assert!(Job::parse("atrium-prerender/1\tlen=1\tnet=yes").is_err());
        assert!(Job::parse("atrium-prerender/2\tlen=1").is_err());
        assert!(Job::parse(&format!("atrium-prerender/1\tlen={}", MAX_DOC + 1)).unwrap_err().contains("limit"));
        assert!(Job { base: Some("a\tb".into()), ..Job::default() }.header().is_err());
    }

    #[test]
    fn a_job_reads_header_then_exactly_its_document() {
        let j = Job { len: 5, ..Job::default() };
        let mut input = j.header().unwrap().into_bytes();
        input.extend_from_slice(b"hello<rest is for the fetch replies>");
        let mut r = &input[..];
        let (got, doc) = read_job(&mut r).unwrap();
        assert_eq!((got.len, doc.as_slice()), (5, &b"hello"[..]));
        assert!(r.starts_with(b"<rest"), "the stream must be left at the first reply");
    }

    #[test]
    fn fetches_are_relayed_and_the_report_recording_and_blobs_collected() {
        let bytes = b"\x89PNG fake";
        let addr = format!("blake3:{}", blake3::hash(bytes).to_hex());
        let mut out = b"F https://a.example/app.js\nR\t1\t2\nE\tsomething\n".to_vec();
        out.extend(format!("BLOB {addr} {}\n", bytes.len()).bytes());
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"RECORDING 4\n{\"a\"V2\tx\t1\t2\nEND\n");
        let (r, sent, urls, blobs) = run(&out);
        let o = r.unwrap();
        assert_eq!(urls, vec!["https://a.example/app.js"]);
        assert_eq!(sent, b"ok 200 0 2 https://a.example/\nhi");
        assert_eq!(o.recording.as_deref(), Some(&b"{\"a\""[..]));
        assert_eq!(blobs, vec![addr]);
        assert_eq!(o.report, "R\t1\t2\nE\tsomething\nV2\tx\t1\t2\n");
        assert!(o.ended && !o.timed_out);
    }

    #[test]
    fn launcher_chatter_on_stdout_is_refused_not_skipped() {
        let (r, ..) = run(b"org_atrium_navigator_converter__7: created\nR\t1\nEND\n");
        assert!(r.unwrap_err().contains("launcher chatter"));
    }

    #[test]
    fn a_blob_under_someone_elses_address_is_a_lie() {
        let other = format!("blake3:{}", blake3::hash(b"the real image").to_hex());
        let (r, _, _, blobs) = run(format!("BLOB {other} 4\nevilEND\n").as_bytes());
        assert!(r.unwrap_err().contains("hashes to"));
        assert!(blobs.is_empty(), "nothing is stored under a lying address");
    }

    #[test]
    fn stated_lengths_over_their_limits_are_refused_unread() {
        for l in [format!("BLOB blake3:00 {}\n", MAX_BLOB + 1), format!("RECORDING {}\n", MAX_RECORDING + 1)] {
            assert!(run(l.as_bytes()).0.unwrap_err().contains("limit"), "{l}");
        }
        // A LINE past MAX_LINE is still a protocol error (a peer that never
        // ends a line is refused at the bound, not buffered forever).
        let long = format!("F {}\n", "a".repeat(MAX_LINE + 100));
        assert!(run(long.as_bytes()).0.is_err());
    }

    #[test]
    fn a_child_that_stops_early_has_not_ended_and_a_timeout_is_reported() {
        let o = run(b"R\t1\n").0.unwrap();
        assert!(!o.ended);
        let o = run(b"TIMEOUT\n").0.unwrap();
        assert!(o.timed_out && !o.ended);
    }

    #[test]
    fn a_report_line_longer_than_a_url_is_a_report_line() {
        let long = format!("E\t{}\nEND\n", "x".repeat(40 * 1024));
        let o = run(long.as_bytes()).0.unwrap();
        assert!(o.ended && o.report.len() > 40 * 1024);
        // …and an oversized fetch REQUEST is refused as that fetch — without
        // being made — while the conversion goes on.
        let f = format!("F https://a.example/{}\nEND\n", "a".repeat(MAX_URL));
        let (r, sent, urls, _) = run(f.as_bytes());
        assert!(r.unwrap().ended);
        assert!(urls.is_empty(), "the fetcher was never asked");
        assert!(String::from_utf8_lossy(&sent).starts_with("err refused: a "), "{}", String::from_utf8_lossy(&sent));
    }

    #[test]
    fn a_child_that_stops_reading_is_not_a_protocol_error() {
        // Its stdin write fails (it exited on its own deadline); what it said
        // on stdout still decides the outcome.
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> { Err(std::io::ErrorKind::BrokenPipe.into()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let stdin: Mutex<Option<Box<dyn Write + Send>>> = Mutex::new(Some(Box::new(Gone)));
        let mut asked = 0;
        let o = drive(&mut &b"F https://a.example/1\nF https://a.example/2\nTIMEOUT\n"[..], &stdin,
                      &mut |_| { asked += 1; b"err no\n".to_vec() }, &mut |_, _| {}).unwrap();
        assert!(o.timed_out && !o.ended);
        assert_eq!((o.fetches, asked), (2, 1), "no fetch is made for a child that is gone");
    }

    #[test]
    fn a_fetch_after_input_was_closed_fails() {
        let stdin: Mutex<Option<Box<dyn Write + Send>>> = Mutex::new(None);
        let r = drive(&mut &b"F https://a.example/\n"[..], &stdin, &mut |_| vec![], &mut |_, _| {});
        assert!(r.unwrap_err().contains("closed"));
    }

    #[test]
    fn report_tags_are_the_protocols_own() {
        for l in ["R\t1", "V2\tx", "E\t", "G\t0\t0"] { assert!(is_report_line(l), "{l}") }
        for l in ["", "R", "r\t1", "RECORD\t1", "hello world", "jail: created"] { assert!(!is_report_line(l), "{l}") }
    }

    #[test]
    fn this_test_process_is_not_jailed() {
        // On the Mac and on an unjailed FreeBSD host alike: the require-
        // confinement check must be able to say no.
        assert_eq!(kernel_says_jailed(), Ok(false));
    }
}
