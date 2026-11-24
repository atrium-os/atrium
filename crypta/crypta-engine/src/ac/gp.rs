//! GlobalProtect's direct gateway mode inside the corporate VPN jail.

use super::*;
use crypta_ac::auth::Field;
use crypta_gp::{gateway as gp, ssl};

const KEEPALIVE: Duration = Duration::from_secs(10);
const MAX_HTTP_WIRE: usize = http::MAX_HEAD + 4 * http::MAX_BODY;
const DEAD: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Direct,
    Portal,
    Selected,
}

pub(super) struct HipRefresh<'a> {
    jar: &'a mut Jar,
    session: &'a gp::Session,
    due: Instant,
}

impl HipRefresh<'_> {
    pub(super) fn tick(&mut self, engine: &mut Engine, addr: Ipv4Addr) -> Result<Duration, Stop> {
        let started = Instant::now();
        if started < self.due {
            return Ok(Duration::ZERO);
        }
        let interval = engine.gp_hip(self.jar, self.session, addr)?;
        self.due = Instant::now() + interval;
        Ok(started.elapsed())
    }
}

enum LinkEnd {
    Stop(Stop),
    Expired,
    EspLost,
}

impl From<Stop> for LinkEnd {
    fn from(stop: Stop) -> Self {
        Self::Stop(stop)
    }
}

fn gateway_io(e: io::Error) -> Stop {
    Stop::Again(format!("GlobalProtect gateway I/O: {e}"))
}

fn refused(why: &str) -> Stop {
    Stop::Refused(why.into())
}

impl Engine {
    fn gp_post(
        &mut self,
        jar: &mut Jar,
        path: &str,
        body: &str,
    ) -> Result<(u16, Zeroizing<Vec<u8>>), Stop> {
        let host = if self.st.gateway.port() == 443 {
            self.st.host.clone()
        } else {
            format!("{}:{}", self.st.host, self.st.gateway.port())
        };
        let mut request = Zeroizing::new(format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: PAN GlobalProtect\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
             Accept-Encoding: identity\r\nConnection: close\r\n",
            body.len(),
        ));
        // This jar goes only to the configured gateway's TLS control endpoints.
        let cookies = jar.header();
        if !cookies.is_empty() {
            request.push_str("Cookie: ");
            request.push_str(&cookies);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        request.push_str(body);
        let mut stream = self.connect()?;
        stream.send(request.as_bytes()).map_err(gateway_io)?;
        let mut buf = Zeroizing::new(Vec::new());
        let deadline = Instant::now() + IO_TIMEOUT;
        loop {
            if buf.len() > MAX_HTTP_WIRE {
                return Err(refused("GlobalProtect HTTP response is too large"));
            }
            if let Some((h, start)) =
                http::head(&buf).map_err(|_| refused("invalid GlobalProtect HTTP response"))?
            {
                if h.status != 200 {
                    return Ok((h.status, Zeroizing::new(Vec::new())));
                }
                if let Some((body, _)) = http::body(&h, &buf[start..])
                    .map_err(|_| refused("invalid GlobalProtect HTTP body"))?
                {
                    jar.take(&h);
                    return Ok((h.status, Zeroizing::new(body)));
                }
            }
            if !gp_receive(&mut stream, &mut buf, deadline)? {
                return Err(Stop::Again(
                    "the GlobalProtect gateway closed its response".into(),
                ));
            }
        }
    }

    fn gp_auth(
        &mut self,
        jar: &mut Jar,
        portal: bool,
        durable: bool,
    ) -> Result<(Zeroizing<Vec<u8>>, bool), Stop> {
        let title = if portal {
            "GlobalProtect portal"
        } else {
            "GlobalProtect gateway"
        };
        let password = if durable {
            "password"
        } else {
            "gp_password_once"
        };
        let authority = format!("{}:{}", self.st.host, self.st.gateway.port());
        let mut form = Form {
            id: "globalprotect".into(),
            title: title.into(),
            message: authority.clone(),
            error: String::new(),
            banner: String::new(),
            fields: vec![
                Field {
                    name: "username".into(),
                    label: "Username".into(),
                    kind: Kind::Text,
                },
                Field {
                    name: password.into(),
                    label: "Password".into(),
                    kind: Kind::Secret,
                },
            ],
        };
        let challenge_form = Form {
            title: title.into(),
            message: format!("{authority}\nEnter the requested verification code."),
            fields: vec![Field {
                name: "gp_response".into(),
                label: "Verification code".into(),
                kind: Kind::Otp,
            }],
            ..form.clone()
        };
        let (prelogin, login) = if portal {
            (gp::PORTAL_PRELOGIN, gp::PORTAL_CONFIG)
        } else {
            (gp::PRELOGIN, gp::LOGIN)
        };
        for _ in 0..3 {
            *jar = Jar::default();
            let (status, body) = self.gp_post(jar, prelogin, "")?;
            if status != 200 {
                return Err(refused("GlobalProtect prelogin failed"));
            }
            if let gp::Prelogin::Saml { login: url } = gp::prelogin(&body).map_err(refused)? {
                // The portal's SAML sign-in already named the person: its
                // cookie for this gateway, once, before a second window.
                if let Some((user, cookie)) = self.gp_portal_cookie.take().filter(|_| !portal) {
                    let body = gp::saml_sign_in(&self.st.host, &self.st.hostname, &user, gp::SamlKind::PortalUserAuthCookie, &cookie);
                    let (status, result) = self.gp_post(jar, login, &body)?;
                    if status == 200 && gp::challenge(&result).map_err(refused)?.is_none() {
                        log("GlobalProtect gateway: signed in with the portal's SAML sign-in");
                        return Ok((result, false));
                    }
                    log("GlobalProtect gateway: the portal's sign-in was not taken; single sign-on again");
                    *jar = Jar::default();
                    let (status, body) = self.gp_post(jar, prelogin, "")?;
                    if status != 200 {
                        return Err(refused("GlobalProtect prelogin failed"));
                    }
                    let gp::Prelogin::Saml { login: url } = gp::prelogin(&body).map_err(refused)? else {
                        return Err(refused("GlobalProtect gateway changed how it signs in"));
                    };
                    return self.gp_saml(jar, login, url);
                }
                return self.gp_saml(jar, login, url);
            }
            let mut kept = false;
            let values = self.answers(&form, &mut kept)?;
            let user = gp_field(&values, "username")?;
            let body = gp::password_login(
                &self.st.host,
                &self.st.hostname,
                user,
                gp_field(&values, password)?,
            );
            let (mut status, mut result) = self.gp_post(jar, login, &body)?;
            let mut rounds = 0;
            while status == 200 {
                let Some(input) = gp::challenge(&result).map_err(refused)? else {
                    return Ok((result, kept));
                };
                if rounds == 3 {
                    return Err(refused(
                        "GlobalProtect gateway kept asking for verification codes",
                    ));
                }
                rounds += 1;
                let answer = self.answers(&challenge_form, &mut kept)?;
                let body = gp::sign_in(
                    &self.st.host,
                    &self.st.hostname,
                    user,
                    gp_field(&answer, "gp_response")?,
                    input,
                );
                (status, result) = self.gp_post(jar, login, &body)?;
            }
            match status {
                401 | 403 | 512 => form.error = "The gateway refused those credentials.".into(),
                _ => return Err(refused("GlobalProtect login failed")),
            }
        }
        Err(refused("GlobalProtect sign-in refused"))
    }

    /// SAML (vpn.md §9.3): the person signs in at the IdP in the
    /// Navigator's VPN sign-in window; the portal's answer at the end
    /// carries a token Arca's courier keeps for Crypta, never the page.
    /// The engine takes it once and logs in with it instead of a password.
    fn gp_saml(&mut self, jar: &mut Jar, login: &str, url: String) -> Result<(Zeroizing<Vec<u8>>, bool), Stop> {
        let final_url = format!("https://{}:{}{}", self.st.host, self.st.gateway.port(), gp::SAML_ACS);
        self.ctl.send(&FromEngine::Sso { login: url, final_url, token_cookie: gp::SAML_TOKEN.into() });
        let done = self.ctl.wait(PROMPT_WAIT, |m| match m {
            ToEngine::SsoDone(ok) => Some(*ok),
            _ => None,
        })?;
        if done != Some(true) {
            return Err(refused("single sign-on did not finish"));
        }
        let tok = self
            .secrets
            .value(&Line::new("sso"))
            .map_err(|e| refused(&format!("the sign-on token could not be had from Arca: {e}")))?;
        let (kind, user, token) = gp::saml_token(tok.as_bytes()).map_err(refused)?;
        let body = gp::saml_sign_in(&self.st.host, &self.st.hostname, &user, kind, &token);
        self.gp_saml_user = Some(user);
        let (status, result) = self.gp_post(jar, login, &body)?;
        match status {
            200 if gp::challenge(&result).map_err(refused)?.is_none() => Ok((result, false)),
            200 => Err(refused("GlobalProtect asked for a code after single sign-on")),
            401 | 403 | 512 => Err(refused("the gateway did not accept the single sign-on")),
            _ => Err(refused("GlobalProtect login failed")),
        }
    }

    fn gp_discover(&mut self, jar: &mut Jar) -> Result<(), Stop> {
        self.gp_saml_user = None;
        let (body, _) = self.gp_auth(jar, true, false)?;
        let choices = gp::gateways(&body).map_err(refused)?;
        // Only after a SAML sign-in: a password portal's gateway asks again.
        let portal_cookie = match self.gp_saml_user.take() {
            Some(user) => gp::portal_cookie(&body).map_err(refused)?.map(|c| (user, c)),
            None => None,
        };
        let form = Form {
            id: "globalprotect-gateway".into(),
            title: "Choose a GlobalProtect gateway".into(),
            message: String::new(),
            error: String::new(),
            banner: String::new(),
            fields: vec![Field {
                name: "gp_gateway".into(),
                label: "Gateway".into(),
                kind: Kind::Choice(choices),
            }],
        };
        let mut kept = false;
        let selection = self.answers(&form, &mut kept)?;
        let selected = gp_field(&selection, "gp_gateway")?;
        if selected != format!("{}:{}", self.st.host, self.st.gateway.port()) {
            return Err(refused(
                "GlobalProtect gateway selection requires a new engine jail",
            ));
        }
        // The same origin: the portal's cookie may stand in for SAML there.
        self.gp_portal_cookie = portal_cookie;
        Ok(())
    }

    fn gp_login(&mut self, jar: &mut Jar, durable: bool) -> Result<gp::Session, Stop> {
        let (body, kept) = self.gp_auth(jar, false, durable)?;
        let session = gp::login(&body).map_err(refused)?;
        if durable && kept && !self.secrets.ok(&Line::new("commit")) {
            log("the GlobalProtect password could not be saved");
        }
        if durable
            && !self
                .secrets
                .ok(&Line::new("cookie-set").with_hex("value", session.stored().as_bytes()))
        {
            log("the GlobalProtect session could not be saved");
        }
        Ok(session)
    }

    fn gp_configure(
        &mut self,
        jar: &mut Jar,
        session: &gp::Session,
    ) -> Result<gp::Config, LinkEnd> {
        let (status, body) = self.gp_post(jar, gp::CONFIG, &session.config(&self.st.hostname))?;
        match status {
            200 => {}
            401 | 403 => return Err(LinkEnd::Expired),
            _ => return Err(refused("GlobalProtect getconfig failed").into()),
        }
        gp::config(&body, self.st.mtu).map_err(|why| refused(why).into())
    }

    fn gp_hip(
        &mut self,
        jar: &mut Jar,
        session: &gp::Session,
        addr: Ipv4Addr,
    ) -> Result<Duration, Stop> {
        let (status, body) = self.gp_post(
            jar,
            gp::HIP_CHECK,
            &session.hip(&self.st.hostname, addr, None),
        )?;
        if status != 200 {
            return Err(refused("GlobalProtect HIP check failed"));
        }
        let interval = gp::hip_interval(&body).map_err(refused)?;
        if gp::hip_check(&body).map_err(refused)? {
            self.ctl.send(&FromEngine::HipFacts);
            let facts = self
                .ctl
                .wait(IO_TIMEOUT, |message| match message {
                    ToEngine::HipFacts(facts) => Some(*facts),
                    _ => None,
                })?
                .ok_or_else(|| refused("Crypta could not verify local HIP facts"))?;
            let os = self
                .hip_os
                .as_deref()
                .ok_or_else(|| refused("Could not determine the local OS for HIP"))?;
            let report = session
                .hip_report(&self.st.hostname, addr, os, facts)
                .map_err(refused)?;
            if self.approved_hip.as_deref() != Some(&report.xml) {
                let mut nonce = [0u8; 16];
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
                    .map_err(|_| refused("Could not create a HIP approval choice"))?;
                let approval = format!("{:032x}", u128::from_be_bytes(nonce));
                let review = Form {
                    id: "globalprotect-hip".into(),
                    title: "Review GlobalProtect device report".into(),
                    message: format!("Send to {}:{}", self.st.host, self.st.gateway.port()),
                    error: String::new(),
                    banner: report.review.clone(),
                    fields: vec![Field {
                        name: "gp_hip_approval".into(),
                        label: "Device report".into(),
                        kind: Kind::Choice(vec![(approval.clone(), "Approve report".into())]),
                    }],
                };
                self.ctl.pending.retain(|message| !matches!(message, ToEngine::Answer(_)));
                self.ctl.send(&FromEngine::Prompt(prompt_of(&review)));
                let answer = self.ctl.wait(PROMPT_WAIT, |message| match message {
                    ToEngine::Rebind => Some(Err(Stop::Again("the network changed".into()))),
                    ToEngine::Answer(None) => Some(Err(refused("GlobalProtect device report was not approved"))),
                    ToEngine::Answer(Some(fields)) => match fields.as_slice() {
                        [(name, Value::Text(value))] if name == "gp_hip_approval" && value == &approval => {
                            Some(Ok(()))
                        }
                        _ => None,
                    },
                    _ => None,
                })?;
                answer.ok_or_else(|| refused("GlobalProtect device report was not approved"))??;
                self.approved_hip = Some(report.xml.clone());
            }
            let (status, body) = self.gp_post(
                jar,
                gp::HIP_REPORT,
                &session.hip(&self.st.hostname, addr, Some(&report)),
            )
            .map_err(|stop| match stop {
                Stop::Again(why) => Stop::Again(format!("GlobalProtect HIP submission failed: {why}")),
                stop => stop,
            })?;
            if status != 200 {
                return Err(refused("GlobalProtect gateway refused the HIP report"));
            }
            gp::hip_accepted(&body).map_err(refused)?;
        }

        Ok(interval)
    }

    fn gp_open_tls(&mut self, session: &gp::Session) -> Result<(tls::Tls, Vec<u8>), LinkEnd> {
        let mut stream = self.connect()?;
        stream
            .send(session.tunnel_request().as_bytes())
            .map_err(gateway_io)?;
        let mut buf = Vec::new();
        let deadline = Instant::now() + IO_TIMEOUT;
        while buf.len() < gp::START_TUNNEL.len() {
            if !gp_receive(&mut stream, &mut buf, deadline)? {
                return Err(Stop::Again(
                    "the GlobalProtect gateway closed the tunnel request".into(),
                )
                .into());
            }
            if !gp::START_TUNNEL.starts_with(&buf[..buf.len().min(gp::START_TUNNEL.len())]) {
                return Err(refused("GlobalProtect TLS tunnel refused").into());
            }
        }
        stream
            .s
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(gateway_io)?;
        Ok((stream, buf.split_off(gp::START_TUNNEL.len())))
    }

    pub(super) fn run_gp(&mut self, mode: Mode) -> Result<(), String> {
        let mut portal = mode == Mode::Portal;
        let durable = mode == Mode::Direct;
        let mut session = if durable && self.st.resume {
            // Check the gateway before releasing its session from Arca.
            match self.connect() {
                Ok(_) => self
                    .secrets
                    .value(&Line::new("cookie"))
                    .ok()
                    .and_then(|raw| gp::Session::restore(&raw)),
                Err(Stop::Refused(why)) => return Err(why),
                Err(_) => None,
            }
        } else {
            None
        };
        let mut jar = Jar::default();
        let mut delay = 1;
        let mut tls_only = false;
        let mut esp_spis = Vec::new();
        loop {
            let result = (|| {
                if session.is_none() {
                    if portal {
                        self.gp_discover(&mut jar)?;
                        portal = false;
                    }
                    session = Some(self.gp_login(&mut jar, durable)?);
                }
                let active = session.as_ref().unwrap();
                let mut config = self.gp_configure(&mut jar, active)?;
                let interval = self.gp_hip(&mut jar, active, config.push.addr)?;
                let refresh = HipRefresh {
                    jar: &mut jar,
                    session: active,
                    due: Instant::now() + interval,
                };
                let mut esp = None;
                if let (Some(keys), Some(socket)) = (config.esp.take(), self.esp.as_ref()) {
                    let spis = (keys.transmit.spi(), keys.receive.spi());
                    let fresh = esp_spis.len() < 256
                        && !esp_spis
                            .iter()
                            .any(|&(tx, rx)| spis.0 == tx || spis.1 == rx);
                    if !tls_only && fresh {
                        esp_spis.push(spis);
                        match super::gp_esp::Transport::start(
                            socket,
                            keys,
                            config.push.addr,
                            &mut self.ctl,
                        ) {
                            Ok(t) => {
                                // IPv4/UDP, the ESP header and IV, padding and authentication tag.
                                config.push.mtu = config.push.mtu.min(1420);
                                esp = Some(t);
                            }
                            Err(Stop::Told) => return Err(Stop::Told.into()),
                            Err(_) => log("GlobalProtect ESP probe failed; using TLS"),
                        }
                    }
                }
                let tls = if esp.is_none() {
                    Some(self.gp_open_tls(active)?)
                } else {
                    None
                };
                self.ctl.send(&FromEngine::Pushed(config.push.clone()));
                if self
                    .ctl
                    .wait(IO_TIMEOUT, |m| matches!(m, ToEngine::Applied).then_some(()))?
                    .is_none()
                {
                    return Err(
                        refused("GlobalProtect tunnel proposal was not acknowledged").into(),
                    );
                }
                self.ctl.send(&FromEngine::Up);
                delay = 1;
                if let Some(esp) = esp {
                    log("GlobalProtect data transport: ESP");
                    let ended = self.gp_pump_esp(esp, &config, refresh);
                    return Err(match ended {
                        Ok(stop) => LinkEnd::Stop(stop),
                        Err(()) => LinkEnd::EspLost,
                    });
                }
                let (t, first) = tls.expect("one data transport");
                log("GlobalProtect data transport: TLS");
                tls_only = false;
                Err::<(), _>(LinkEnd::Stop(self.gp_pump(t, &config, first, refresh)))
            })();
            match result {
                Ok(()) => return Ok(()),
                Err(LinkEnd::EspLost) => {
                    tls_only = true;
                    self.ctl.send(&FromEngine::Down {
                        why: "GlobalProtect ESP lost; reconnecting over TLS".into(),
                    });
                }
                Err(LinkEnd::Expired) => {
                    session = None;
                    jar = Jar::default();
                    if durable {
                        let _ = self.secrets.ok(&Line::new("cookie-drop"));
                    }
                    self.ctl.send(&FromEngine::Down {
                        why: "GlobalProtect session expired".into(),
                    });
                    if self
                        .pause("GlobalProtect session expired", &mut delay)
                        .is_err()
                    {
                        break;
                    }
                }
                Err(LinkEnd::Stop(Stop::Told)) => break,
                Err(LinkEnd::Stop(Stop::Refused(why))) => return Err(why),
                Err(LinkEnd::Stop(Stop::Again(why))) => {
                    self.ctl.send(&FromEngine::Down { why: why.clone() });
                    if self.pause(&why, &mut delay).is_err() {
                        break;
                    }
                }
            }
        }
        if let Some(active) = session.filter(|_| self.ctl.stopped) {
            let _ = self.gp_post(&mut jar, gp::LOGOUT, &active.logout(&self.st.hostname));
        }
        Ok(())
    }

    fn gp_pump(
        &mut self,
        mut t: tls::Tls,
        config: &gp::Config,
        mut buf: Vec<u8>,
        mut refresh: HipRefresh<'_>,
    ) -> Stop {
        let p = &config.push;
        let tun = self.tun.as_raw_fd();
        let mut last_rx = Instant::now();
        let renew = last_rx + config.renew_after;
        let mut last_stats = last_rx;
        let mut probe = false;
        let keepalive = ssl::encode(ssl::Frame::Keepalive).expect("fixed keepalive");
        let mut packet = [0u8; 4 + ssl::MAX_PACKET + 1];
        loop {
            let mut tail = buf.as_slice();
            loop {
                let (frame, remaining) = match ssl::decode(tail) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(_) => return refused("invalid GlobalProtect tunnel frame"),
                };
                last_rx = Instant::now();
                match frame {
                    ssl::Frame::Keepalive if !std::mem::take(&mut probe) => {
                        if let Err(e) = t.send(&keepalive) {
                            return gateway_io(e);
                        }
                    }
                    ssl::Frame::Keepalive => {}
                    ssl::Frame::Packet(ip) if ip[0] >> 4 == 4 && ip.len() <= usize::from(p.mtu) => {
                        packet[..4].copy_from_slice(&AF_INET_HDR);
                        packet[4..4 + ip.len()].copy_from_slice(ip);
                        match self.tun.write(&packet[..4 + ip.len()]) {
                            Ok(n) if n == 4 + ip.len() => self.rx += ip.len() as u64,
                            _ => self.dropped += 1,
                        }
                    }
                    ssl::Frame::Packet(_) => self.dropped += 1,
                }
                tail = remaining;
            }
            let consumed = buf.len() - tail.len();
            buf.drain(..consumed);
            let mut fds = [
                libc::pollfd {
                    fd: self.ctl.s.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: t.s.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: tun,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: all three descriptors are owned by the engine.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 500) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return gateway_io(err);
            }
            if fds[0].revents != 0 {
                if let Err(stop) = self.ctl.fill(0) {
                    return stop;
                }
            }
            if self
                .ctl
                .pending
                .drain(..)
                .any(|m| matches!(m, ToEngine::Rebind))
            {
                return Stop::Again("the network changed".into());
            }
            if fds[1].revents != 0 {
                match t.recv(&mut buf) {
                    Ok(true) => {}
                    Ok(false) => return Stop::Again("the GlobalProtect tunnel closed".into()),
                    Err(e) => return gateway_io(e),
                }
            }
            if fds[2].revents != 0 {
                match self.tun.read(&mut packet) {
                    Ok(0) => return Stop::Told,
                    Ok(n)
                        if n >= 24
                            && n <= 4 + usize::from(p.mtu)
                            && packet[..4] == AF_INET_HDR
                            && packet[16..20] == p.addr.octets() =>
                    {
                        match ssl::encode(ssl::Frame::Packet(&packet[4..n])) {
                            Ok(frame) => {
                                if let Err(e) = t.send(&frame) {
                                    return gateway_io(e);
                                }
                                self.tx += (n - 4) as u64;
                            }
                            Err(_) => self.dropped += 1,
                        }
                    }
                    Ok(_) => self.dropped += 1,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) => {}
                    Err(e) => return Stop::Refused(format!("reading the tunnel: {e}")),
                }
            }
            match refresh.tick(self, p.addr) {
                Ok(elapsed) => last_rx += elapsed,
                Err(stop) => return stop,
            }
            let now = Instant::now();
            if now >= renew {
                return Stop::Again("renewing the GlobalProtect tunnel".into());
            }
            if now.duration_since(last_rx) >= DEAD {
                return Stop::Again("the GlobalProtect gateway stopped answering".into());
            }
            if now.duration_since(last_rx) >= KEEPALIVE && !probe {
                if let Err(e) = t.send(&keepalive) {
                    return gateway_io(e);
                }
                probe = true;
            }
            if now.duration_since(last_stats) >= STATS_EVERY {
                last_stats = now;
                self.ctl.send(&FromEngine::Stats {
                    rx: self.rx,
                    tx: self.tx,
                    dropped: self.dropped,
                });
            }
        }
    }
}

fn gp_receive(stream: &mut tls::Tls, buf: &mut Vec<u8>, deadline: Instant) -> Result<bool, Stop> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Stop::Again("GlobalProtect response timed out".into()));
    }
    stream.s.set_read_timeout(Some(left)).map_err(gateway_io)?;
    stream.recv(buf).map_err(gateway_io)
}

fn gp_field<'a>(values: &'a [(String, Zeroizing<String>)], name: &str) -> Result<&'a str, Stop> {
    values
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| refused("GlobalProtect answer missing"))
}

pub(super) fn hip_os() -> Option<String> {
    // Obtain the kernel's actual name and release before Capsicum. No
    // nodename, host ID or hardware identifiers are copied into the report.
    let mut raw = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname writes a utsname on success.
    if unsafe { libc::uname(raw.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: successful uname initialized the structure and NUL-terminated its fields.
    let raw = unsafe { raw.assume_init() };
    let text = |p| unsafe { std::ffi::CStr::from_ptr(p) }.to_str().ok();
    Some(format!(
        "{} {}",
        text(raw.sysname.as_ptr())?,
        text(raw.release.as_ptr())?
    ))
}
