//! Running the converter process from an app (§5.2): the foreign file goes
//! in on stdin (after the password, when there is one), the native file
//! comes back on stdout and the fidelity notes on stderr.
//!
//! A password-protected file is decrypted inside the converter, after it
//! has sandboxed itself, so the app never parses the encrypted container.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The longest password passed to the converter, in bytes.
pub const MAX_PASSWORD: usize = 1024;

/// Why a conversion gave no document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The file is password-protected and no password was given.
    NeedsPassword,
    /// The password given doesn't open it.
    WrongPassword,
    /// Anything else, in words.
    Failed(String),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::NeedsPassword => write!(f, "it is protected with a password"),
            ConvertError::WrongPassword => write!(f, "that password doesn't open it"),
            ConvertError::Failed(w) => write!(f, "{w}"),
        }
    }
}

/// The lines the converter writes on stderr when a password is the matter.
pub const NEEDS_PASSWORD: &str = "needs-password";
pub const WRONG_PASSWORD: &str = "wrong-password";

/// The converter's program: `OFFICINA_CONVERT`, else next to this
/// program, else on the path.
pub fn converter() -> PathBuf {
    std::env::var_os("OFFICINA_CONVERT").map(PathBuf::from).unwrap_or_else(|| {
        let here = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("officina-convert")));
        here.filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("officina-convert"))
    })
}

/// What goes on the converter's stdin before the file when there is a
/// password: its length (u32, little-endian) and its UTF-8 bytes.
pub fn password_prefix(password: &str) -> Vec<u8> {
    let p = password.as_bytes();
    let p = &p[..p.len().min(MAX_PASSWORD)];
    let mut v = (p.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(p);
    v
}

/// Runs `officina-convert KIND` over `data`: the native file and the
/// fidelity notes, in words.
pub fn convert(kind: &str, data: &[u8], password: Option<&str>) -> Result<(Vec<u8>, Vec<String>), ConvertError> {
    let exe = converter();
    let mut cmd = Command::new(&exe);
    cmd.arg(kind);
    if password.is_some() {
        cmd.arg("--password");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ConvertError::Failed(format!("the converter ({}) didn't start: {e}", exe.display())))?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let prefix = password.map(password_prefix).unwrap_or_default();
    std::thread::scope(|sc| {
        sc.spawn(move || {
            let _ = stdin.write_all(&prefix).and_then(|_| stdin.write_all(data));
        });
        let e = sc.spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let mut out = Vec::new();
        let r = stdout.read_to_end(&mut out);
        let status = child.wait();
        let errs = e.join().unwrap_or_default();
        let mut notes = Vec::new();
        let mut failure = None;
        for l in errs.lines() {
            if let Some(n) = l.strip_prefix("note\t") {
                if let Some((kind, count, what, ex)) = crate::report::Report::parse_line(n) {
                    let ex = if ex.is_empty() { String::new() } else { format!(" (e.g. {ex})") };
                    notes.push(if count == 1 { format!("{}: {what}{ex}", kind.word()) } else { format!("{}: {count} × {what}{ex}", kind.word()) });
                }
            } else if let Some(w) = l.strip_prefix("error\t") {
                failure = Some(match w {
                    NEEDS_PASSWORD => ConvertError::NeedsPassword,
                    WRONG_PASSWORD => ConvertError::WrongPassword,
                    w => ConvertError::Failed(w.to_string()),
                });
            }
        }
        match (r, status) {
            (Ok(_), Ok(st)) if st.success() => Ok((out, notes)),
            _ => Err(failure.unwrap_or_else(|| ConvertError::Failed("the converter stopped".into()))),
        }
    })
}

/// A file in `kind` (docx, xlsx, xlsm, pptx, odt, ods, odp), as written,
/// protected with `password`: Agile encryption for OOXML, AES-256-CBC
/// parts for ODF.
pub fn protect(kind: &str, file: &[u8], password: &str) -> Result<Vec<u8>, crate::crypt::CryptError> {
    match kind {
        "odt" | "ods" | "odp" => crate::odfcrypt::encrypt(file, password),
        _ => Ok(crate::crypt::encrypt(file, password)),
    }
}

/// `input` (a foreign file, as read) opened for a reader of `kind`:
/// decrypted when it is password-protected. Runs in the converter, after
/// it has sandboxed itself. The error is the stderr line's text.
pub fn unprotect(kind: &str, input: Vec<u8>, password: Option<&str>) -> Result<Vec<u8>, String> {
    use crate::crypt::CryptError;
    let odf = matches!(kind, "odt" | "ods" | "odp");
    let locked = if odf { crate::odfcrypt::is_encrypted_odf(&input) } else { crate::crypt::is_encrypted_ooxml(&input) };
    if !locked {
        return Ok(input);
    }
    let Some(pw) = password else { return Err(NEEDS_PASSWORD.into()) };
    let r = if odf { crate::odfcrypt::decrypt(&input, pw) } else { crate::crypt::decrypt(&input, pw) };
    r.map_err(|e| match e {
        CryptError::WrongPassword => WRONG_PASSWORD.into(),
        e => e.to_string(),
    })
}
