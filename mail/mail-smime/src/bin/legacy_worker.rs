//! The disabled integration build's networkless text-only reader.

#[cfg(target_os = "freebsd")]
fn main() {
    if freebsd::run().is_err() {
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "freebsd"))]
fn main() {
    std::process::exit(1);
}

#[cfg(target_os = "freebsd")]
mod freebsd {
    use std::io::{self, Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use zeroize::Zeroizing;

    use mail_smime::{envelope, legacy_wire as wire};

    pub fn run() -> io::Result<()> {
        let mut jailed: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&jailed);
        // SAFETY: read the integer into a correctly sized buffer, then
        // irreversibly restrict the process to its inherited descriptors.
        let confined = unsafe {
            libc::sysctlbyname(
                c"security.jail.jailed".as_ptr(),
                (&mut jailed as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null(),
                0,
            ) == 0
                && jailed == 1
                && libc::cap_enter() == 0
        };
        if !confined {
            return Err(io::Error::other("legacy reader needs a FreeBSD jail"));
        }
        // SAFETY: the fixed Portcullis worker owns its stdin socket.
        let mut socket = unsafe { UnixStream::from_raw_fd(0) };
        socket.write_all(wire::WORKER_READY)?;
        if execute(&mut socket).is_err() {
            socket.write_all(&[0])?;
        }
        Ok(())
    }

    fn execute(socket: &mut UnixStream) -> io::Result<()> {
        let invalid = || io::Error::other(wire::FAILURE);
        let cert = wire::read(socket, wire::MAX_CERT)?;
        let input = wire::read(socket, wire::MAX_CMS)?;
        let cms = envelope::enveloped_data(&input).map_err(|_| invalid())?;
        let (fingerprint, key_bytes, wrapped) =
            envelope::legacy_recipient(&cms, &cert).ok_or_else(invalid)?;
        socket.write_all(&[1, key_bytes as u8])?;
        socket.write_all(&fingerprint)?;
        wire::write(socket, &wrapped, 512)?;

        let mut key = Zeroizing::new([0u8; 32]);
        match socket.read_exact(&mut key[..key_bytes]) {
            Ok(()) => {}
            // The broker can close without sending a candidate, for example
            // when this persona has no enrollment. EOF completes that call.
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        }
        let plain =
            Zeroizing::new(envelope::decrypt(&cms, &key[..key_bytes]).map_err(|_| invalid())?);
        let text = wire::plain_text(&plain).ok_or_else(invalid)?;
        socket.write_all(&[1])?;
        wire::write(socket, text.as_bytes(), wire::MAX_TEXT)
    }
}
