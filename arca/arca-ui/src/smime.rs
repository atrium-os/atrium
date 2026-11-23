//! Operator provisioning entry point for the trusted Arca UI identity.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;

use arca_store::model::caps;
use arca_store::secret::LockedBuf;

use crate::arca::{Arcad, Error};

fn read_der(mut file: File, max: usize) -> Result<LockedBuf, Error> {
    let mode = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if mode < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stat = file.metadata()?;
    if mode & libc::O_ACCMODE != libc::O_RDONLY
        || !stat.is_file()
        || !(1..=max as u64).contains(&stat.len())
    {
        return Err(Error::Malformed);
    }
    let mut data = LockedBuf::zeroed(stat.len() as usize)?;
    file.read_exact(data.as_mut())?;
    let mut extra = LockedBuf::zeroed(1)?;
    if file.read(extra.as_mut())? != 0 {
        return Err(Error::Malformed);
    }
    Ok(data)
}

fn persona(name: &str) -> &str {
    if name == "personal" {
        ""
    } else {
        name
    }
}

fn fingerprint(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn run(arca: &Arcad, args: &[String]) -> ExitCode {
    let result = match args {
        [mode, user, name, label, certificate] if mode == "smime-import" => (|| {
            let cert_file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                .open(certificate)?;
            let cert = read_der(cert_file, arcad::proto::SMIME_CERT_MAX)?;
            let key = read_der(
                File::from(io::stdin().as_fd().try_clone_to_owned()?),
                caps::VALUE,
            )?;
            let digest = arca.provision_smime(user, persona(name), label, &cert, &key)?;
            println!(
                "{}",
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            Ok(())
        })(),
        [mode, user, name, digest] if mode == "smime-forget" => fingerprint(digest)
            .ok_or(Error::Malformed)
            .and_then(|digest| arca.forget_smime(user, persona(name), &digest)),
        _ => {
            eprintln!(
                "Usage: arca-ui smime-import USER PERSONA LABEL CERTIFICATE.der < PRIVATE-KEY.pk8"
            );
            eprintln!("       arca-ui smime-forget USER PERSONA FINGERPRINT");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let message = match error {
                Error::Status(arcad::proto::status::LOCKED) => "the user’s Arca store is locked",
                Error::Status(arcad::proto::status::EXISTS) => "that key is already registered for this persona",
                Error::Status(arcad::proto::status::DENIED) => "Arca denied the provisioning operation",
                Error::Status(arcad::proto::status::FULL) => "the persona’s S/MIME certificate limit is full",
                Error::Malformed | Error::Status(arcad::proto::status::MALFORMED) => "expected a matching DER certificate and RSA PKCS#8 key from bounded read-only files",
                _ => "the Arca provisioning operation failed",
            };
            eprintln!("S/MIME: {message}");
            ExitCode::FAILURE
        }
    }
}
