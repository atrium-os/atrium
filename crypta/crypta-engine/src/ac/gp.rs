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

enum LinkEnd {
    Stop(Stop),
    Expired,
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
            gp::prelogin(&body).map_err(refused)?;
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

    fn gp_discover(&mut self, jar: &mut Jar) -> Result<(), Stop> {
        let (body, _) = self.gp_auth(jar, true, false)?;
        let choices = gp::gateways(&body).map_err(refused)?;
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

    fn gp_open(
        &mut self,
        jar: &mut Jar,
        session: &gp::Session,
    ) -> Result<(tls::Tls, gp::Config, Vec<u8>), LinkEnd> {
        let (status, body) = self.gp_post(jar, gp::CONFIG, &session.config(&self.st.hostname))?;
        match status {
            200 => {}
            401 | 403 => return Err(LinkEnd::Expired),
            _ => return Err(refused("GlobalProtect getconfig failed").into()),
        }
        let config = gp::config(&body, self.st.mtu).map_err(refused)?;
        let (status, body) = self.gp_post(
            jar,
            gp::HIP_CHECK,
            &session.hip(&self.st.hostname, config.push.addr),
        )?;
        if status != 200 {
            return Err(refused("GlobalProtect HIP check failed").into());
        }
        gp::hip_check(&body).map_err(refused)?;

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
        Ok((stream, config, buf.split_off(gp::START_TUNNEL.len())))
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
                let (t, config, first) = self.gp_open(&mut jar, active)?;
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
                let ended = self.gp_pump(t, &config, first);
                Err::<(), _>(LinkEnd::Stop(ended))
            })();
            match result {
                Ok(()) => return Ok(()),
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

    fn gp_pump(&mut self, mut t: tls::Tls, config: &gp::Config, mut buf: Vec<u8>) -> Stop {
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
