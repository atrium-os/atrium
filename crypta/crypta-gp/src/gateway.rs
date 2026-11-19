//! Direct gateway exchange for GlobalProtect. Wire fields follow the
//! independent PAN_GlobalProtect_protocol_doc.md packet traces.

use std::fmt::Write;
use std::net::Ipv4Addr;
use std::time::Duration;

use crypta_ac::xml::{self, Element};
use crypta_proto::{engine::Pushed, net::Prefix4};
use zeroize::{Zeroize, Zeroizing};

pub const PRELOGIN: &str = "/ssl-vpn/prelogin.esp?tmp=tmp&clientVer=4100&clientos=Linux";
pub const LOGIN: &str = "/ssl-vpn/login.esp";
pub const CONFIG: &str = "/ssl-vpn/getconfig.esp";
pub const HIP_CHECK: &str = "/ssl-vpn/hipreportcheck.esp";
pub const LOGOUT: &str = "/ssl-vpn/logout.esp";
pub const START_TUNNEL: &[u8] = b"START_TUNNEL";
const SSL_PATH: &str = "/ssl-tunnel-connect.sslvpn";

pub type Result<T> = std::result::Result<T, &'static str>;

pub fn form(fields: &[(&str, &str)]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::new());
    for (name, value) in fields {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(name);
        out.push('=');
        for b in value.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => {
                    let _ = write!(out, "%{b:02X}");
                }
            }
        }
    }
    out
}

pub fn password_login(host: &str, computer: &str, user: &str, password: &str) -> Zeroizing<String> {
    form(&[
        ("prot", "https:"),
        ("server", host),
        ("computer", computer),
        ("user", user),
        ("passwd", password),
        ("ok", "Login"),
        ("direct", "yes"),
        ("jnlpReady", "jnlpReady"),
        ("clientVer", "4100"),
        ("clientos", "Linux"),
        ("os-version", "Atrium"),
        ("clientgpversion", "Atrium"),
    ])
}

pub struct Session {
    cookie: Zeroizing<String>,
    user: String,
    portal: String,
    domain: String,
    preferred_ip: Option<Ipv4Addr>,
}

impl Session {
    /// Short, versioned record for this profile's Arca session item.
    pub fn stored(&self) -> Zeroizing<String> {
        let ip = self
            .preferred_ip
            .map(|ip| ip.to_string())
            .unwrap_or_default();
        Zeroizing::new(format!(
            "gp1\n{}\n{}\n{}\n{}\n{ip}",
            self.cookie.as_str(),
            self.user,
            self.portal,
            self.domain
        ))
    }

    pub fn restore(raw: &str) -> Option<Self> {
        let [version, cookie, user, portal, domain, ip] =
            raw.split('\n').collect::<Vec<_>>().try_into().ok()?;
        if version != "gp1" {
            return None;
        }
        Self::new(cookie, user, portal, domain, ip).ok()
    }

    fn new(cookie: &str, user: &str, portal: &str, domain: &str, ip: &str) -> Result<Self> {
        if cookie.is_empty()
            || cookie.len() > 1024
            || cookie.chars().any(char::is_control)
            || user.is_empty()
            || [user, portal, domain]
                .iter()
                .any(|s| s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err("invalid GlobalProtect tunnel session");
        }
        Ok(Self {
            cookie: Zeroizing::new(cookie.to_owned()),
            user: user.into(),
            portal: portal.into(),
            domain: domain.into(),
            preferred_ip: match ip {
                "" | "(null)" | "0.0.0.0" => None,
                _ => Some(
                    ip.parse()
                        .map_err(|_| "invalid GlobalProtect preferred IP")?,
                ),
            },
        })
    }

    pub fn config(&self, computer: &str) -> Zeroizing<String> {
        let ip = self.preferred_ip.map(|a| a.to_string()).unwrap_or_default();
        form(&[
            ("user", &self.user),
            ("authcookie", &self.cookie),
            ("portal", &self.portal),
            ("domain", &self.domain),
            ("computer", computer),
            ("preferred-ip", &ip),
            ("client-type", "1"),
            ("protocol-version", "p1"),
            ("clientos", "Linux"),
            ("os-version", "Atrium"),
            ("app-version", "Atrium"),
            ("internal", "no"),
        ])
    }

    pub fn hip(&self, computer: &str, ip: Ipv4Addr) -> Zeroizing<String> {
        form(&[
            ("user", &self.user),
            ("authcookie", &self.cookie),
            ("computer", computer),
            ("portal", &self.portal),
            ("domain", &self.domain),
            ("client-role", "global-protect-full"),
            ("client-ip", &ip.to_string()),
            ("md5", ""),
        ])
    }

    pub fn logout(&self, computer: &str) -> Zeroizing<String> {
        form(&[
            ("user", &self.user),
            ("authcookie", &self.cookie),
            ("portal", &self.portal),
            ("domain", &self.domain),
            ("computer", computer),
            ("os-version", "Atrium"),
        ])
    }

    pub fn tunnel_request(&self) -> Zeroizing<String> {
        let query = form(&[("user", &self.user), ("authcookie", &self.cookie)]);
        Zeroizing::new(format!(
            "GET {SSL_PATH}?{} HTTP/1.1\r\n\r\n",
            query.as_str()
        ))
    }
}

fn document(bytes: &[u8]) -> Result<Element> {
    xml::parse(bytes).map_err(|_| "invalid GlobalProtect XML")
}

fn unique<'a>(root: &'a Element, name: &str) -> Result<Option<&'a Element>> {
    let mut v = root.children.iter().filter(|e| e.name == name);
    let first = v.next();
    if v.next().is_some() {
        return Err("duplicate GlobalProtect field");
    }
    Ok(first)
}

fn scalar<'a>(root: &'a Element, name: &str) -> Result<Option<&'a str>> {
    match unique(root, name)? {
        Some(e) if e.children.is_empty() => Ok(Some(e.text.trim()).filter(|s| !s.is_empty())),
        Some(_) => Err("invalid GlobalProtect field"),
        None => Ok(None),
    }
}

pub fn prelogin(bytes: &[u8]) -> Result<()> {
    let root = document(bytes)?;
    if root.name != "prelogin-response" || scalar(&root, "status")? != Some("Success") {
        return Err("GlobalProtect prelogin refused");
    }
    if scalar(&root, "saml-auth-method")?.is_some() || scalar(&root, "saml-request")?.is_some() {
        return Err("GlobalProtect SAML sign-in is unavailable");
    }
    Ok(())
}

/// Clear JNLP argument text after extracting the session, including any
/// echoed authentication material.
fn clear_xml(e: &mut Element) {
    e.text.zeroize();
    for (_, value) in &mut e.attrs {
        value.zeroize();
    }
    for child in &mut e.children {
        clear_xml(child);
    }
}

pub fn login(bytes: &[u8]) -> Result<Session> {
    let mut root = document(bytes)?;
    let answer = session(&root);
    clear_xml(&mut root);
    answer
}

fn session(root: &Element) -> Result<Session> {
    if root.name != "jnlp" {
        return Err("GlobalProtect sign-in refused or requires an unsupported challenge");
    }
    let app =
        unique(root, "application-desc")?.ok_or("missing GlobalProtect application description")?;
    let args: Vec<_> = app.children_named("argument").collect();
    if args.len() < 13 || args.iter().any(|e| !e.children.is_empty()) {
        return Err("invalid GlobalProtect login arguments");
    }
    let value = |i: usize| match args[i].text.trim() {
        "(null)" => "",
        s => s,
    };
    if value(10) != "tunnel" {
        return Err("invalid GlobalProtect tunnel session");
    }
    Session::new(
        value(1),
        value(4),
        value(3),
        value(7),
        args.get(13).map_or("", |a| a.text.trim()),
    )
}

fn entries<'a>(root: &'a Element, name: &str, limit: usize) -> Result<Vec<&'a str>> {
    let Some(list) = unique(root, name)? else {
        return Ok(Vec::new());
    };
    if list.children.len() > limit
        || !list.text.trim().is_empty()
        || list
            .children
            .iter()
            .any(|e| e.name != "member" || !e.children.is_empty() || e.text.trim().is_empty())
    {
        return Err("invalid GlobalProtect list");
    }
    Ok(list.children.iter().map(|e| e.text.trim()).collect())
}

pub struct Config {
    pub push: Pushed,
    pub renew_after: Duration,
}

pub fn config(bytes: &[u8], fallback_mtu: u16) -> Result<Config> {
    let mut root = document(bytes)?;
    let answer = proposal(&root, fallback_mtu);
    clear_xml(&mut root);
    answer
}

fn proposal(root: &Element, fallback_mtu: u16) -> Result<Config> {
    if root.name != "response" || root.attr("status").is_some_and(|s| s != "success") {
        return Err("GlobalProtect configuration refused");
    }
    if scalar(root, "need-tunnel")? == Some("no") {
        return Err("GlobalProtect gateway did not offer a tunnel");
    }
    if scalar(root, "no-direct-access-to-local-network")? == Some("yes") {
        return Err("GlobalProtect gateway requires local-network lockdown");
    }
    if scalar(root, "ssl-tunnel-url")? != Some(SSL_PATH) {
        return Err("GlobalProtect gateway has no supported TLS tunnel path");
    }
    let ip = |name| {
        scalar(root, name)?
            .ok_or("missing GlobalProtect address")?
            .parse::<Ipv4Addr>()
            .map_err(|_| "invalid GlobalProtect address")
    };
    let addr = ip("ip-address")?;
    let mask = u32::from(ip("netmask")?);
    let prefix = mask.leading_ones() as u8;
    if mask != crypta_proto::net::mask(prefix) {
        return Err("non-contiguous GlobalProtect netmask");
    }
    let mtu = match scalar(root, "mtu")? {
        None | Some("0") => fallback_mtu,
        Some(mtu) => mtu
            .parse::<u16>()
            .map_err(|_| "invalid GlobalProtect MTU")?,
    }
    .clamp(1280, 1500);
    let includes = entries(root, "access-routes", 1024)?
        .into_iter()
        .map(Prefix4::parse)
        .collect::<Option<Vec<_>>>()
        .ok_or("invalid GlobalProtect include route")?;
    // The current atrium-net proposal cannot represent exclusions. Refuse
    // these pushes so the resulting policy cannot differ from the gateway's.
    if !entries(root, "exclude-access-routes", 256)?.is_empty() {
        return Err("GlobalProtect split exclusions are unsupported");
    }
    let full = includes.is_empty() || includes.iter().any(|r| r.len == 0);
    let dns = entries(root, "dns", 3)?
        .into_iter()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| "invalid GlobalProtect DNS")?;
    let domains = entries(root, "dns-suffix", 64)?;
    if domains.iter().any(|domain| domain.len() > 253) {
        return Err("GlobalProtect DNS suffix is too long");
    }
    let timeout = match scalar(root, "timeout")? {
        None | Some("0") => 3600,
        Some(seconds) => seconds
            .parse::<u64>()
            .map_err(|_| "invalid GlobalProtect timeout")?,
    };
    // Refresh before the gateway expires the TLS tunnel.
    let renew_after = Duration::from_secs(timeout.clamp(15, 86400) * 9 / 10);
    Ok(Config {
        renew_after,
        push: Pushed {
            addr,
            prefix,
            mtu,
            includes,
            excludes: Vec::new(),
            full,
            dns,
            domains: domains.into_iter().map(str::to_owned).collect(),
            banner: String::new(),
            ignored: Vec::new(),
        },
    })
}

pub fn hip_check(bytes: &[u8]) -> Result<()> {
    let root = document(bytes)?;
    if root.name == "response" && root.attr("status").is_none_or(|s| s == "success") {
        match scalar(&root, "hip-report-needed")? {
            Some("no") => return Ok(()),
            Some("yes") => return Err("GlobalProtect gateway requires a HIP report"),
            _ => {}
        }
    }
    Err("unrecognized GlobalProtect HIP check")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelogin_distinguishes_password_and_saml() {
        assert!(
            prelogin(b"<prelogin-response><status>Success</status></prelogin-response>").is_ok()
        );
        assert!(prelogin(b"<prelogin-response><status>Success</status><saml-request>opaque</saml-request></prelogin-response>").is_err());
        assert!(prelogin(b"<prelogin-response><status>Success</status><status>Failure</status></prelogin-response>").is_err());
    }

    #[test]
    fn jnlp_session_survives_round_trip_and_encodes_query() {
        let arguments = [
            "",
            "cookie+&amp;",
            "",
            "portal",
            "Alice A",
            "ldap",
            "vsys1",
            "corp",
            "",
            "",
            "tunnel",
            "-1",
            "4100",
            "10.0.0.9",
        ];
        let mut jnlp = String::from("<jnlp><application-desc>");
        for arg in arguments {
            let _ = write!(jnlp, "<argument>{arg}</argument>");
        }
        jnlp.push_str("</application-desc></jnlp>");
        let session = login(jnlp.as_bytes()).unwrap();
        let session = Session::restore(&session.stored()).unwrap();
        assert!(session.config("atrium").contains("preferred-ip=10.0.0.9"));
        assert_eq!(
            session.tunnel_request().as_str(),
            "GET /ssl-tunnel-connect.sslvpn?user=Alice+A&authcookie=cookie%2B%26 HTTP/1.1\r\n\r\n"
        );
        assert!(
            password_login("vpn.example", "atrium", "alice", "é&+").contains("passwd=%C3%A9%26%2B")
        );
    }

    #[test]
    fn proposal_rejects_unrepresentable_policy() {
        let response = r#"<response status="success">
            <ssl-tunnel-url>/ssl-tunnel-connect.sslvpn</ssl-tunnel-url>
            <ip-address>10.0.0.2</ip-address><netmask>255.255.255.0</netmask><timeout>60</timeout>
            <access-routes><member>10.0.0.0/24</member></access-routes>
            <dns><member>10.0.0.1</member></dns><dns-suffix><member>corp.test</member></dns-suffix>
        </response>"#;
        let settings = config(response.as_bytes(), 1406).unwrap();
        assert_eq!(settings.renew_after, Duration::from_secs(54));
        let push = settings.push;
        assert_eq!(push.prefix, 24);
        assert_eq!(push.dns, vec![Ipv4Addr::new(10, 0, 0, 1)]);
        assert!(!push.full);
        for altered in [
            response.replace("/ssl-tunnel-connect.sslvpn", "//another.example"),
            response.replace("255.255.255.0", "255.0.255.0"),
            response.replace("</response>", "<exclude-access-routes><member>10.0.0.7</member></exclude-access-routes></response>"),
        ] {
            assert!(config(altered.as_bytes(), 1406).is_err());
        }
        assert!(
            hip_check(b"<response><hip-report-needed>no</hip-report-needed></response>").is_ok()
        );
        assert!(
            hip_check(b"<response><hip-report-needed>yes</hip-report-needed></response>").is_err()
        );
    }
}
