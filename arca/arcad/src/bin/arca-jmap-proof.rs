//! Signed, disabled lab worker and the trusted operator's synthetic probe.
//! The worker serves its fixture on the jail's private loopback. Its only
//! Arca channel is the inherited Portcullis stream; it never opens arcad.sock.

use arca_store::codec::Writer;
use arcad::proto::{self, op, status};
use mail_proto::jmap;
use mail_smime::legacy_wire as wire;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HOST: &str = "jmap.fixture.invalid";
const ORIGIN: &str = "https://jmap.fixture.invalid";
const AUTH: &str = "Authorization: Bearer atrium-synthetic-jmap-only";
const DOWNLOAD: &str = "/download/a1/blob/message.eml";
const TIMEOUT: Duration = Duration::from_secs(2);

fn invalid() -> io::Error {
    io::Error::other("synthetic JMAP transport refused")
}

fn line(input: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    let n = input.take(1024).read_line(&mut line)?;
    if n == 0 || !line.ends_with("\r\n") {
        return Err(invalid());
    }
    line.truncate(line.len() - 2);
    Ok(line)
}

fn header(input: &mut impl BufRead) -> io::Result<(usize, bool)> {
    let mut bytes = None;
    let mut authorized = false;
    for _ in 0..16 {
        let item = line(input)?;
        if item.is_empty() {
            return Ok((bytes.unwrap_or(0), authorized));
        }
        if let Some(n) = item.strip_prefix("Content-Length: ") {
            if bytes.is_some() {
                return Err(invalid());
            }
            bytes = Some(
                n.parse::<usize>()
                    .ok()
                    .filter(|n| *n <= wire::MAX_RECEIVED)
                    .ok_or_else(invalid)?,
            );
        }
        authorized |= item == AUTH;
        if item.to_ascii_lowercase().starts_with("transfer-encoding:") {
            return Err(invalid());
        }
    }
    Err(invalid())
}

fn timeouts(stream: &TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))
}

fn outbound(
    address: SocketAddr,
    config: Arc<ClientConfig>,
    mode: u8,
    method: &str,
    path: &str,
    body: &[u8],
) -> io::Result<Vec<u8>> {
    let mut tcp = TcpStream::connect_timeout(&address, TIMEOUT)?;
    timeouts(&tcp)?;
    let host = if mode == 2 {
        "wrong.fixture.invalid"
    } else {
        HOST
    };
    let name = ServerName::try_from(host).map_err(io::Error::other)?;
    let mut conn = ClientConnection::new(config, name).map_err(io::Error::other)?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)?;
    }
    let mut tls = StreamOwned::new(conn, tcp);
    write!(tls, "{method} {path} HTTP/1.1\r\nHost: {HOST}\r\n{AUTH}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    tls.write_all(body)?;
    tls.flush()?;
    let mut input = BufReader::new(tls);
    if line(&mut input)? != "HTTP/1.1 200 OK" {
        return Err(invalid());
    }
    let (n, _) = header(&mut input)?;
    if n == 0 {
        return Err(invalid());
    }
    let mut result = vec![0; n];
    input.read_exact(&mut result)?;
    Ok(result)
}

fn answer(listener: &TcpListener, config: Arc<ServerConfig>, mode: u8) -> io::Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    let tcp = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => return Err(e),
        }
    };
    timeouts(&tcp)?;
    tcp.set_nonblocking(false)?;
    let conn = ServerConnection::new(config).map_err(io::Error::other)?;
    let mut input = BufReader::new(StreamOwned::new(conn, tcp));
    let request = line(&mut input)?;
    let (n, authorized) = header(&mut input)?;
    let mut body = vec![0; n];
    input.read_exact(&mut body)?;
    if !authorized {
        return Err(invalid());
    }
    let message = wire::fixture_message();
    let response = match request.as_str() {
        "GET /.well-known/jmap HTTP/1.1" if body.is_empty() => format!(
            r#"{{
            "username":"login@fixture.invalid",
            "capabilities":{{"urn:ietf:params:jmap:core":{{}},"urn:ietf:params:jmap:mail":{{}}}},
            "accounts":{{"a1":{{"name":"fixture","isPersonal":true,
                "accountCapabilities":{{"urn:ietf:params:jmap:mail":{{}}}}}}}},
            "primaryAccounts":{{"urn:ietf:params:jmap:mail":"a1"}},
            "apiUrl":"{ORIGIN}/api",
            "downloadUrl":"{ORIGIN}/download/{{accountId}}/{{blobId}}/{{name}}",
            "uploadUrl":"{ORIGIN}/upload/{{accountId}}","state":"s"
        }}"#
        )
        .into_bytes(),
        "POST /api HTTP/1.1" if body == jmap::get_emails("a1", &["message".into()]) => {
            let account = if mode == 1 {
                "substituted-account"
            } else {
                "a1"
            };
            format!(
                r#"{{"methodResponses":[["Email/get",{{"accountId":"{account}","state":"s",
                "notFound":[],"list":[{{"id":"message","blobId":"blob",
                "mailboxIds":{{"inbox":true}},"size":{}}}]}},"g"]]}}"#,
                message.len()
            )
            .into_bytes()
        }
        "GET /download/a1/blob/message.eml HTTP/1.1" if body.is_empty() => message,
        _ => return Err(invalid()),
    };
    let tls = input.get_mut();
    write!(
        tls,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    tls.write_all(&response)?;
    tls.flush()
}

fn https_fixture(dir: &Path, nonce: &[u8], mode: u8) -> io::Result<Vec<u8>> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(std::fs::read(dir.join("ca.der"))?))
        .map_err(io::Error::other)?;
    let client = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let private_key =
        PrivateKeyDer::try_from(std::fs::read(dir.join("key.pk8"))?).map_err(io::Error::other)?;
    let server = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(std::fs::read(dir.join("server.der"))?)],
                private_key,
            )
            .map_err(io::Error::other)?,
    );
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    std::thread::scope(|scope| {
        let responder = scope.spawn(move || {
            for _ in 0..if mode == 2 { 1 } else { 3 } {
                answer(&listener, server.clone(), mode)?;
            }
            Ok::<_, io::Error>(())
        });
        let session = outbound(
            address,
            client.clone(),
            mode,
            "GET",
            "/.well-known/jmap",
            &[],
        );
        if mode == 2 {
            let Err(error) = session else {
                return Err(invalid());
            };
            if !matches!(
                error
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<rustls::Error>()),
                Some(rustls::Error::InvalidCertificate(_))
            ) {
                return Err(error);
            }
            let server_result = responder.join().map_err(|_| invalid())?;
            if server_result.is_ok() {
                return Err(invalid());
            }
            return Ok(vec![2]);
        }
        let session = session?;
        let metadata = outbound(
            address,
            client.clone(),
            mode,
            "POST",
            "/api",
            &jmap::get_emails("a1", &["message".into()]),
        )?;
        let message = outbound(address, client, mode, "GET", DOWNLOAD, &[])?;
        responder.join().map_err(|_| invalid())??;
        let mut reply = Writer::new();
        reply.raw(nonce);
        reply.str(ORIGIN);
        reply.bytes(&session);
        reply.bytes(&metadata);
        reply.str(&format!("{ORIGIN}{DOWNLOAD}"));
        reply.bytes(&message);
        Ok(reply.buf)
    })
}

#[cfg(target_os = "freebsd")]
fn worker() -> io::Result<()> {
    use std::os::fd::FromRawFd;
    let mut jailed: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&jailed);
    // The dispatcher always installs a private Unix stream on fd 0.
    if unsafe {
        libc::sysctlbyname(
            c"security.jail.jailed".as_ptr(),
            (&mut jailed as *mut libc::c_int).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || jailed != 1
    {
        return Err(invalid());
    }
    let mut control = unsafe { UnixStream::from_raw_fd(0) };
    control.set_read_timeout(Some(TIMEOUT))?;
    control.set_write_timeout(Some(TIMEOUT))?;
    let request = wire::read(&mut control, 33)?;
    let (nonce, [mode @ (0 | 1 | 2)]) = request.split_at_checked(32).ok_or_else(invalid)? else {
        return Err(invalid());
    };
    let result = https_fixture(Path::new("/fixture"), nonce, *mode)?;
    wire::write(&mut control, &result, 64 << 10)
}

// One lab request. `None` is Arca's public denial.
fn request(path: &str, uid: u32, mode: u8) -> io::Result<Option<Vec<u8>>> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(12)))?;
    let mut body = uid.to_le_bytes().to_vec();
    body.push(mode);
    proto::write_request(&mut stream, op::U_SMIME_SYNTHETIC_JMAP, &body)?;
    // This lab opcode has a public result. The unprivileged Mail probe
    // must be able to read a denial without allocating locked memory.
    let mut header = [0; 5];
    stream.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header[..4].try_into().map_err(io::Error::other)?);
    match (header[4], length) {
        (status::DENIED, 1) => Ok(None),
        (status::OK, 2..=16) => {
            let mut witness = vec![0; length as usize - 1];
            stream.read_exact(&mut witness)?;
            Ok(Some(witness))
        }
        _ => Err(invalid()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Mode 0 seals a new account (witness 0) or uses the sealed one (4), and
// names it by a tag that matches only the same instance and receipt key.
fn probe(path: &str, uid: u32, expect: &str) -> io::Result<()> {
    let expected = match expect {
        "new" => 0,
        "kept" => 4,
        _ => return Err(invalid()),
    };
    let refused = |label: &str| io::Error::other(format!("JMAP proof witness {label} refused"));
    match request(path, uid, 0)? {
        Some(witness) if witness.len() == 9 && witness[0] == expected => {
            println!("MAIL_JMAP_PROOF_RECEIPT_OK");
            println!("MAIL_JMAP_PROOF_ACCOUNT_{}", expect.to_uppercase());
            println!("MAIL_JMAP_PROOF_AUTHORITY={}", hex(&witness[1..]));
        }
        _ => return Err(refused("RECEIPT_OK")),
    }
    for (mode, label) in [(1, "WRONG_ACCOUNT_REFUSED"), (2, "TLS_NAME_REFUSED")] {
        if request(path, uid, mode)? != Some(vec![mode]) {
            return Err(refused(label));
        }
        println!("MAIL_JMAP_PROOF_{label}");
    }
    Ok(())
}

fn revoke(path: &str, uid: u32, count: u8) -> io::Result<()> {
    if request(path, uid, 3)? != Some(vec![3, count]) {
        return Err(io::Error::other("JMAP proof revocation refused"));
    }
    println!("MAIL_JMAP_PROOF_REVOKED={count}");
    Ok(())
}

fn deny(path: &str, uid: u32) -> io::Result<()> {
    for mode in [0, 3] {
        if request(path, uid, mode)?.is_some() {
            return Err(io::Error::other("JMAP proof allowed a denied caller"));
        }
    }
    println!("MAIL_JMAP_PROOF_DIRECT_DENIED");
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let uid = |uid: &String| uid.parse::<u32>().map_err(io::Error::other);
    match args.as_slice() {
        [cmd, path, id, expect] if cmd == "probe" => probe(path, uid(id)?, expect),
        [cmd, path, id, count] if cmd == "revoke" => {
            revoke(path, uid(id)?, count.parse().map_err(io::Error::other)?)
        }
        [cmd, path, id] if cmd == "deny" => deny(path, uid(id)?),
        #[cfg(target_os = "freebsd")]
        [] => worker(),
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_store::codec::Reader;

    #[test]
    fn synthetic_https_validates_server_name_and_carries_read_only_jmap() {
        let dir =
            std::env::var_os("ATRIUM_JMAP_TEST_CERTS").expect("synthetic TLS fixture directory");
        let dir = Path::new(&dir);
        for mode in [0, 1] {
            let bytes = https_fixture(dir, &[3; 32], mode).unwrap();
            let mut reader = Reader::new(&bytes);
            assert_eq!(reader.array::<32>().unwrap(), [3; 32]);
            assert_eq!(reader.str(128).unwrap(), ORIGIN);
            assert_eq!(
                jmap::session(reader.bytes(4096).unwrap()).unwrap().account,
                "a1"
            );
            let emails = jmap::answers(reader.bytes(4096).unwrap()).unwrap();
            assert_eq!(
                jmap::emails(&emails, "a1", &["message".into()]).is_ok(),
                mode == 0
            );
            assert_eq!(reader.str(512).unwrap(), format!("{ORIGIN}{DOWNLOAD}"));
            assert_eq!(
                reader.bytes(wire::MAX_RECEIVED).unwrap(),
                wire::fixture_message()
            );
            reader.end().unwrap();
        }
        assert_eq!(https_fixture(dir, &[3; 32], 2).unwrap(), [2]);
    }
}
