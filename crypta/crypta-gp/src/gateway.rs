//! Direct gateway exchange for GlobalProtect. Wire fields follow the
//! independent PAN_GlobalProtect_protocol_doc.md packet traces.

use std::fmt::Write;
use std::net::Ipv4Addr;
use std::time::Duration;

use crypta_ac::xml::{self, Element};
use crypta_proto::{engine::Pushed, net::Prefix4};
use zeroize::{Zeroize, Zeroizing};

pub const PRELOGIN: &str = "/ssl-vpn/prelogin.esp?tmp=tmp&clientVer=4100&clientos=Linux";
pub const PORTAL_PRELOGIN: &str =
    "/global-protect/prelogin.esp?tmp=tmp&clientVer=4100&clientos=Linux";
pub const PORTAL_CONFIG: &str = "/global-protect/getconfig.esp";
pub const LOGIN: &str = "/ssl-vpn/login.esp";
pub const CONFIG: &str = "/ssl-vpn/getconfig.esp";
pub const HIP_REPORT: &str = "/ssl-vpn/hipreport.esp";
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
    sign_in(host, computer, user, password, "")
}

pub fn sign_in(
    host: &str,
    computer: &str,
    user: &str,
    password: &str,
    input: &str,
) -> Zeroizing<String> {
    form(&[
        ("prot", "https:"),
        ("server", host),
        ("computer", computer),
        ("user", user),
        ("passwd", password),
        ("inputStr", input),
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
            ("enc-algo", "aes-128-cbc"),
            ("hmac-algo", "sha1"),
            ("clientos", "Linux"),
            ("os-version", "Atrium"),
            ("app-version", "Atrium"),
            ("internal", "no"),
        ])
    }

    pub fn hip(
        &self,
        computer: &str,
        ip: Ipv4Addr,
        report: Option<&HipReport>,
    ) -> Zeroizing<String> {
        let ip = ip.to_string();
        let mut fields = vec![
            ("user", self.user.as_str()),
            ("authcookie", self.cookie.as_str()),
            ("computer", computer),
            ("portal", self.portal.as_str()),
            ("domain", self.domain.as_str()),
            ("client-role", "global-protect-full"),
            ("client-ip", ip.as_str()),
            ("md5", report.map_or("", |r| r.digest.as_str())),
        ];
        if let Some(report) = report {
            fields.push(("report", &report.xml));
        }
        form(&fields)
    }

    pub fn hip_report(
        &self,
        computer: &str,
        ip: Ipv4Addr,
        os: &str,
        facts: crypta_proto::engine::HipFacts,
    ) -> Result<HipReport> {
        HipReport::new(&self.user, computer, ip, os, facts)
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

/// Read the challenge token from the fixed wire envelopes. The gateway
/// message is opaque and is never interpreted as markup or program code.
pub fn challenge(bytes: &[u8]) -> Result<Option<&str>> {
    if bytes.len() > xml::MAX_DOC {
        return Err("GlobalProtect challenge is too large");
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "invalid GlobalProtect challenge text")?
        .trim();
    let xml = text
        .split_once("?>")
        .filter(|_| text.starts_with("<?xml"))
        .map_or(text, |(_, tail)| tail.trim_start());
    let token = if let Some(body) = xml
        .strip_prefix("<challenge>")
        .and_then(|s| s.strip_suffix("</challenge>"))
    {
        // PAN challenge messages can contain bare ampersands. Skip the
        // message as opaque text and accept just the literal token field.
        let body = body.trim();
        let body = if let Some(message) = body.strip_prefix("<respmsg>") {
            message
                .split_once("</respmsg>")
                .ok_or("invalid GlobalProtect challenge message")?
                .1
                .trim()
        } else {
            body
        };
        body.strip_prefix("<inputstr>")
            .and_then(|s| s.strip_suffix("</inputstr>"))
    } else {
        let legacy = text
            .strip_prefix("<html><head></head><body>")
            .and_then(|s| s.strip_suffix("</body></html>"))
            .unwrap_or(text);
        let (status, assignment, end) =
            if legacy.starts_with("var respStatus = &quot;Challenge&quot;;") {
                (
                    "var respStatus = &quot;Challenge&quot;;",
                    "thisForm.inputStr.value = &quot;",
                    "&quot;;",
                )
            } else {
                (
                    "var respStatus = \"Challenge\";",
                    "thisForm.inputStr.value = \"",
                    "\";",
                )
            };
        let Some(body) = legacy.strip_prefix(status) else {
            return Ok(None);
        };
        let mut values = body
            .lines()
            .filter_map(|line| line.trim().strip_prefix(assignment));
        let first = values.next().and_then(|s| s.strip_suffix(end));
        if values.next().is_some() {
            return Err("duplicate GlobalProtect challenge token");
        }
        first
    };
    token
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 512
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.~+/=".contains(&b))
        })
        .map(Some)
        .ok_or("invalid GlobalProtect challenge token")
}

pub fn gateways(bytes: &[u8]) -> Result<Vec<(String, String)>> {
    let mut root = document(bytes)?;
    let result = gateway_choices(&root);
    clear_xml(&mut root);
    result
}

fn gateway_choices(root: &Element) -> Result<Vec<(String, String)>> {
    if root.name != "policy" {
        return Err("GlobalProtect portal refused configuration");
    }
    let list = unique(root, "gateways")?
        .and_then(|g| g.child("external"))
        .and_then(|g| g.child("list"))
        .ok_or("GlobalProtect portal offered no external gateways")?;
    let mut choices = Vec::new();
    for entry in &list.children {
        if entry.name != "entry" {
            return Err("invalid GlobalProtect gateway list");
        }
        let name = entry
            .attr("name")
            .ok_or("missing GlobalProtect gateway address")?;
        let (host, port) = match name.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or("invalid GlobalProtect gateway port")?,
            ),
            None => (name, 443),
        };
        if host.is_empty()
            || host.len() > 253
            || !host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err("invalid GlobalProtect gateway host");
        }
        if choices.len() == 64 {
            return Err("too many GlobalProtect gateways");
        }
        let address = format!("{}:{port}", host.to_ascii_lowercase());
        let description: String = entry
            .child_text("description")
            .unwrap_or("")
            .chars()
            .filter(|c| {
                !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
            .take(80)
            .collect();
        let label = if description.is_empty() {
            address.clone()
        } else {
            format!("{address} ({description})")
        };
        choices.push((address, label));
    }
    if choices.is_empty() {
        return Err("GlobalProtect portal offered no external gateways");
    }
    Ok(choices)
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
    // Some gateways insert two empty reserved arguments before the mode.
    let mode = match (value(10), args.get(12).map(|a| a.text.trim())) {
        ("tunnel", Some("4100")) => 10,
        ("", Some("tunnel"))
            if value(11).is_empty() && args.get(14).is_some_and(|a| a.text.trim() == "4100") =>
        {
            12
        }
        _ => return Err("invalid GlobalProtect tunnel session"),
    };
    Session::new(
        value(1),
        value(4),
        value(3),
        value(7),
        args.get(mode + 3).map_or("", |a| a.text.trim()),
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

pub const ESP_PORT: u16 = 4501;

pub struct EspConfig {
    pub transmit: super::esp::Key,
    pub receive: super::esp::Key,
    pub probe: Ipv4Addr,
}

pub struct Config {
    pub esp: Option<EspConfig>,
    pub push: Pushed,
    pub renew_after: Duration,
}

pub fn config(bytes: &[u8], fallback_mtu: u16) -> Result<Config> {
    let mut root = document(bytes)?;
    let answer = proposal(&root, fallback_mtu);
    clear_xml(&mut root);
    answer
}

fn esp_config(root: &Element) -> Option<EspConfig> {
    let esp = unique(root, "ipsec").ok()??;
    let field = |name| scalar(esp, name).ok().flatten();
    if field("udp-port")?.parse::<u16>().ok()? != ESP_PORT
        || field("ipsec-mode")? != "esp-tunnel"
        || !matches!(field("enc-algo")?, "aes-128-cbc" | "aes128")
        || field("hmac-algo")? != "sha1"
    {
        return None;
    }
    let spi = |name| u32::from_str_radix(field(name)?.strip_prefix("0x")?, 16).ok();
    let probe: Ipv4Addr = scalar(root, "gw-address").ok()??.parse().ok()?;
    if probe.is_unspecified() || probe.is_multicast() || probe.is_broadcast() {
        return None;
    }
    Some(EspConfig {
        transmit: super::esp::Key::new(
            spi("c2s-spi")?,
            hex_key(esp, "ekey-c2s")?,
            hex_key(esp, "akey-c2s")?,
        )?,
        receive: super::esp::Key::new(
            spi("s2c-spi")?,
            hex_key(esp, "ekey-s2c")?,
            hex_key(esp, "akey-s2c")?,
        )?,
        probe,
    })
}

fn hex_key<const N: usize>(esp: &Element, field: &str) -> Option<Zeroizing<[u8; N]>> {
    let key = unique(esp, field).ok()??;
    if scalar(key, "bits").ok()??.parse::<usize>().ok()? != N * 8 {
        return None;
    }
    let encoded = scalar(key, "val").ok()??.as_bytes();
    if encoded.len() != N * 2 {
        return None;
    }
    let mut out = Zeroizing::new([0; N]);
    for (output, pair) in out.iter_mut().zip(encoded.chunks_exact(2)) {
        *output =
            (char::from(pair[0]).to_digit(16)? * 16 + char::from(pair[1]).to_digit(16)?) as u8;
    }
    Some(out)
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
    let mask = match scalar(root, "netmask")? {
        // A gateway may assign just the client's point-to-point address.
        None => u32::MAX,
        Some(mask) => u32::from(
            mask.parse::<Ipv4Addr>()
                .map_err(|_| "invalid GlobalProtect netmask")?,
        ),
    };
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
        esp: esp_config(root),
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

/// Facts included in a report and displayed for approval. The same
/// values feed both views. Unknown posture categories have no XML entry.
pub struct HipReport {
    pub xml: String,
    digest: String,
    pub review: String,
}

impl HipReport {
    fn new(
        user: &str,
        computer: &str,
        ip: Ipv4Addr,
        os: &str,
        facts: crypta_proto::engine::HipFacts,
    ) -> Result<Self> {
        use md5::{Digest, Md5};
        let ip = ip.to_string();
        let info = [
            ("user-name", "VPN user", user),
            ("host-name", "VPN hostname", computer),
            ("ip-address", "Assigned IPv4", &ip),
        ];
        if info.iter().map(|(_, _, v)| *v).chain([os]).any(|v| {
            v.is_empty()
                || v.len() > 100
                || v.chars().any(|c| {
                    c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
                })
        }) {
            return Err("GlobalProtect HIP facts cannot be displayed safely");
        }
        let mut root = Element::new("hip-report")
            .with_attr("name", "hip-report")
            .with_child(Element::new("hip-report-version").with_text("4"));
        let mut review = String::new();
        for (key, label, value) in info {
            root = root.with_child(Element::new(key).with_text(value));
            let _ = writeln!(review, "{label}  {value}");
        }
        let mut host = Element::new("entry")
            .with_attr("name", "host-info")
            .with_child(Element::new("os").with_text(os))
            .with_child(Element::new("client-version").with_text("Atrium"));
        let _ = writeln!(review, "Operating system  {os}");
        review.push_str("Client  Atrium\n");
        if let Some(enabled) = facts.ipv6_forwarding {
            let value = if enabled { "yes" } else { "no" };
            host = host.with_child(Element::new("ipv6-forwarding").with_text(value));
            let _ = writeln!(review, "IPv6 forwarding  {value}");
        }
        let mut categories = Element::new("categories").with_child(host);
        if let Some(enabled) = facts.pf {
            let value = if enabled { "yes" } else { "no" };
            categories = categories.with_child(
                Element::new("entry")
                    .with_attr("name", "firewall")
                    .with_child(
                        Element::new("entry")
                            .with_child(Element::new("product").with_text("Atrium pf"))
                            .with_child(Element::new("is-enabled").with_text(value)),
                    ),
            );
            let _ = writeln!(review, "Firewall  Atrium pf enabled: {value}");
        }
        if let Some(count) = facts.mounted_geli {
            let count = count.to_string();
            categories = categories.with_child(
                Element::new("entry")
                    .with_attr("name", "disk-encryption")
                    .with_child(
                        Element::new("entry")
                            .with_child(Element::new("product").with_text("FreeBSD GELI"))
                            .with_child(
                                Element::new("mounted-encrypted-providers").with_text(&count),
                            ),
                    ),
            );
            let _ = writeln!(review, "Mounted GELI encrypted providers  {count}");
        }
        if let Some(status) = facts.patch_status {
            let last_check = status.last_check.to_string();
            let mut patch = Element::new("entry")
                .with_attr("name", "patch-management")
                .with_child(Element::new("product").with_text("Atrium Opifex"))
                .with_child(Element::new("last-check-unix").with_text(&last_check));
            let _ = writeln!(review, "Opifex last check (Unix seconds)  {last_check}");
            if let Some(pending) = status.pending {
                let pending = pending.to_string();
                patch =
                    patch.with_child(Element::new("catalog-updates-listed").with_text(&pending));
                let _ = writeln!(review, "Opifex catalog updates listed  {pending}");
            } else {
                patch = patch.with_child(Element::new("last-check-failed").with_text("yes"));
                let _ = writeln!(review, "Opifex last check failed");
            }
            categories = categories.with_child(patch);
        }
        if let Some(locked) = facts.screen_locked {
            let value = if locked { "yes" } else { "no" };
            categories = categories.with_child(
                Element::new("entry")
                    .with_attr("name", "screen-lock")
                    .with_child(Element::new("active-session-locked").with_text(value)),
            );
            let _ = writeln!(review, "Active session locked  {value}");
        }
        root = root.with_child(categories);
        review.push_str("Other posture categories are omitted.");
        // MD5 is the protocol's report identifier, separate from the
        // TLS transport and from the exact-content approval check.
        let digest = format!("{:x}", Md5::digest(root.to_doc().as_bytes()));
        root.children
            .insert(0, Element::new("md5-sum").with_text(&digest));
        Ok(Self {
            xml: root.to_doc(),
            digest,
            review,
        })
    }
}

pub fn hip_check(bytes: &[u8]) -> Result<bool> {
    let root = document(bytes)?;
    if root.name == "response" && root.attr("status").is_none_or(|s| s == "success") {
        match scalar(&root, "hip-report-needed")? {
            Some("no") => return Ok(false),
            Some("yes") => return Ok(true),
            _ => {}
        }
    }
    Err("unrecognized GlobalProtect HIP check")
}

pub fn hip_interval(bytes: &[u8]) -> Result<Duration> {
    let root = document(bytes)?;
    let seconds = match scalar(&root, "hip-report-interval")? {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| "invalid GlobalProtect HIP interval")?,
        None => 600,
    };
    Ok(Duration::from_secs(seconds.clamp(5, 3600)))
}

pub fn hip_accepted(bytes: &[u8]) -> Result<()> {
    let root = document(bytes)?;
    if root.name == "response" && root.attr("status") == Some("success") {
        Ok(())
    } else {
        Err("GlobalProtect gateway refused the HIP report")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hip_review_reports_only_the_supplied_facts() {
        let report = HipReport::new(
            "alice & bob",
            "atrium",
            Ipv4Addr::new(10, 77, 0, 2),
            "FreeBSD 16.0",
            Default::default(),
        )
        .unwrap();
        assert!(report.review.contains("VPN user  alice & bob"));
        let xml = document(report.xml.as_bytes()).unwrap();
        assert_eq!(xml.child_text("user-name"), Some("alice & bob"));
        assert_eq!(xml.child("categories").unwrap().children.len(), 1);
        let changed = HipReport::new(
            "alice & bob",
            "atrium",
            Ipv4Addr::new(10, 77, 0, 3),
            "FreeBSD 16.0",
            Default::default(),
        )
        .unwrap();
        assert_ne!(report.xml, changed.xml);
        assert_ne!(report.digest, changed.digest);
        assert!(HipReport::new(
            "alice\nOS: fake",
            "atrium",
            Ipv4Addr::LOCALHOST,
            "FreeBSD",
            Default::default()
        )
        .is_err());
        assert!(hip_accepted(b"<response status='error'/>").is_err());
    }

    #[test]
    fn hip_records_verified_booleans_and_bounds_gateway_intervals() {
        let facts = crypta_proto::engine::HipFacts {
            pf: Some(true),
            ipv6_forwarding: Some(false),
            ..Default::default()
        };
        let report =
            HipReport::new("alice", "atrium", Ipv4Addr::LOCALHOST, "FreeBSD", facts).unwrap();
        assert!(report.xml.contains("<ipv6-forwarding>no</ipv6-forwarding>"));
        assert!(report
            .xml
            .contains("<product>Atrium pf</product><is-enabled>yes</is-enabled>"));
        assert!(report.review.contains("IPv6 forwarding  no"));
        assert!(report.review.contains("Firewall  Atrium pf enabled: yes"));
        assert_eq!(hip_interval(b"<response/>"), Ok(Duration::from_secs(600)));
        assert_eq!(
            hip_interval(b"<response><hip-report-interval>0</hip-report-interval></response>"),
            Ok(Duration::from_secs(5))
        );
        assert_eq!(
            hip_interval(b"<response><hip-report-interval>999999</hip-report-interval></response>"),
            Ok(Duration::from_secs(3600))
        );
    }

    #[test]
    fn each_new_posture_fact_changes_the_report_and_review() {
        use crypta_proto::engine::{HipFacts, HipPatchStatus};

        let facts = HipFacts {
            mounted_geli: Some(1),
            patch_status: Some(HipPatchStatus {
                last_check: 100,
                pending: Some(2),
            }),
            screen_locked: Some(false),
            ..Default::default()
        };
        let report = |facts| {
            HipReport::new("alice", "atrium", Ipv4Addr::LOCALHOST, "FreeBSD", facts).unwrap()
        };
        let original = report(facts);
        for changed in [
            HipFacts {
                mounted_geli: Some(0),
                ..facts
            },
            HipFacts {
                patch_status: Some(HipPatchStatus {
                    last_check: 100,
                    pending: None,
                }),
                ..facts
            },
            HipFacts {
                screen_locked: Some(true),
                ..facts
            },
        ] {
            let changed = report(changed);
            assert_ne!(changed.xml, original.xml);
            assert_ne!(changed.review, original.review);
        }
    }

    #[test]
    fn portal_offers_only_host_and_port_choices() {
        let policy = |name| {
            format!(
                r#"<policy><gateways><external><list>
            <entry name="{name}"><description>Office</description></entry>
            </list></external></gateways></policy>"#
            )
        };
        let choices = gateways(policy("VPN.Example").as_bytes()).unwrap();
        assert_eq!(
            choices,
            vec![("vpn.example:443".into(), "vpn.example:443 (Office)".into())]
        );
        for bad in [
            "https://example.test",
            "example.test:0",
            "user@example.test",
            "example.test/path",
        ] {
            assert!(gateways(policy(bad).as_bytes()).is_err());
        }
    }

    #[test]
    fn challenge_accepts_a_literal_and_rejects_a_computed_value() {
        let xml =
            b"<challenge><respmsg>code & token</respmsg><inputstr>nonce</inputstr></challenge>";
        assert_eq!(challenge(xml), Ok(Some("nonce")));
        let script = br#"var respStatus = "Challenge";
thisForm.inputStr.value = "literal";"#;
        assert_eq!(challenge(script), Ok(Some("literal")));
        let computed = br#"var respStatus = "Challenge";
thisForm.inputStr.value = getValue();"#;
        assert!(challenge(computed).is_err());
    }

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
            hip_check(b"<response><hip-report-needed>yes</hip-report-needed></response>")
                == Ok(true)
        );
    }
}
