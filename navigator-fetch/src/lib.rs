//! M1 — the Navigator's fetcher (navigator-backend §2, §8).
//!
//! The only component that holds network, and one that cannot name a file:
//! on FreeBSD, `sandbox::enter` puts the process in Capsicum capability mode
//! with a casper `cap_net` channel as its only way out (see `sandbox`). What
//! it can do then is exactly `fetch`: resolve, connect, TLS, one bounded GET,
//! a bounded number of redirects.

pub mod http;
#[cfg(target_os = "freebsd")]
pub mod sandbox;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

pub const MAX_REDIRECTS: usize = 5;
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// How a fetch reaches the network. The sandboxed daemon uses casper; tests
/// and the unsandboxed host use `Direct`.
pub trait Connect {
    fn connect(&self, host: &str, port: u16) -> std::io::Result<TcpStream>;
}

pub struct Direct;
impl Connect for Direct {
    fn connect(&self, host: &str, port: u16) -> std::io::Result<TcpStream> { TcpStream::connect((host, port)) }
}

#[cfg(target_os = "freebsd")]
impl Connect for sandbox::Net {
    fn connect(&self, host: &str, port: u16) -> std::io::Result<TcpStream> { sandbox::Net::connect(self, host, port) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    /// The URL the body came from, after redirects.
    pub url: String,
    pub response: http::Response,
    pub redirects: usize,
}

#[derive(Debug, PartialEq)]
pub enum FetchError {
    BadUrl(String),
    /// Only http and https, only ports 80 and 443 (what casper will resolve),
    /// no credentials in the URL.
    Refused(String),
    Connect(String),
    Tls(String),
    Http(http::HttpError),
    TooManyRedirects,
}

/// Load the trust store. Must happen BEFORE capability mode: afterwards the
/// file cannot be named.
pub fn tls_config(pem: &[u8]) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls_pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    let mut n = 0;
    for c in CertificateDer::pem_slice_iter(pem) {
        let c = c.map_err(|e| format!("trust store: {e:?}"))?;
        if roots.add(c).is_ok() { n += 1 }
    }
    if n == 0 { return Err("trust store holds no usable certificates".into()) }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
        .with_root_certificates(roots).with_no_client_auth();
    Ok(Arc::new(cfg))
}

pub fn fetch(net: &dyn Connect, tls: &Arc<rustls::ClientConfig>, url: &str) -> Result<Fetched, FetchError> {
    let mut current = url::Url::parse(url).map_err(|e| FetchError::BadUrl(e.to_string()))?;
    for redirects in 0..=MAX_REDIRECTS {
        let response = get_once(net, tls, &current)?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            let loc = response.header("location").ok_or(FetchError::Http(http::HttpError::Malformed("redirect without Location")))?;
            let next = current.join(loc).map_err(|e| FetchError::BadUrl(e.to_string()))?;
            // ★ Never downgrade: a redirect from https to http would hand a
            // network attacker the rest of the chain.
            if current.scheme() == "https" && next.scheme() != "https" {
                return Err(FetchError::Refused(format!("redirect downgrades to {next}")));
            }
            current = next;
            continue;
        }
        return Ok(Fetched { url: current.to_string(), response, redirects });
    }
    Err(FetchError::TooManyRedirects)
}

fn get_once(net: &dyn Connect, tls: &Arc<rustls::ClientConfig>, u: &url::Url) -> Result<http::Response, FetchError> {
    let https = match u.scheme() { "https" => true, "http" => false, s => return Err(FetchError::Refused(format!("scheme {s}"))) };
    if !u.username().is_empty() || u.password().is_some() {
        return Err(FetchError::Refused("credentials in URL".into()));
    }
    let host = u.host_str().ok_or_else(|| FetchError::BadUrl("no host".into()))?.to_string();
    let port = u.port_or_known_default().unwrap_or(0);
    if port != 80 && port != 443 { return Err(FetchError::Refused(format!("port {port}"))) }
    let host_header = match u.port() { Some(p) => format!("{host}:{p}"), None => host.clone() };
    let mut path = u.path().to_string();
    if let Some(q) = u.query() { path.push('?'); path.push_str(q) }

    let tcp = net.connect(&host, port).map_err(|e| FetchError::Connect(e.to_string()))?;
    tcp.set_read_timeout(Some(TIMEOUT)).ok();
    tcp.set_write_timeout(Some(TIMEOUT)).ok();
    if https {
        let name = rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| FetchError::Tls(e.to_string()))?;
        let conn = rustls::ClientConnection::new(tls.clone(), name).map_err(|e| FetchError::Tls(e.to_string()))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        http::write_request(&mut s, &host_header, &path).map_err(|e| FetchError::Tls(e.to_string()))?;
        read(&mut s)
    } else {
        let mut s = tcp;
        http::write_request(&mut s, &host_header, &path).map_err(|e| FetchError::Connect(e.to_string()))?;
        read(&mut s)
    }
}

fn read(s: &mut (impl Read + Write)) -> Result<http::Response, FetchError> {
    // A peer closing without TLS close_notify surfaces as UnexpectedEof from
    // rustls; for a Connection: close response whose framing is already
    // complete that is not an error, so the reader decides by framing.
    http::read_response(s).map_err(FetchError::Http)
}
