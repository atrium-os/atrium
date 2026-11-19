//! ESP probe and packet pump. The connected UDP socket is created before
//! Capsicum and can only reach the selected gateway's standard ESP port.

use super::*;
use crypta_gp::{esp, gateway};
use std::net::UdpSocket;

const PROBE_DATA: &[u8] = b"monitor\0\0pan ha ";
const INTERVAL: Duration = Duration::from_secs(10);
const DEAD: Duration = Duration::from_secs(30);

pub(super) struct Transport {
    send: esp::Send,
    recv: esp::Receive,
    src: Ipv4Addr,
    dst: Ipv4Addr,
    id: [u8; 2],
    probe: u16,
}

impl Transport {
    pub(super) fn start(
        socket: &UdpSocket,
        keys: gateway::EspConfig,
        src: Ipv4Addr,
        ctl: &mut Ctl,
    ) -> Result<Self, Stop> {
        let mut id = [0; 2];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut id)
            .map_err(|_| Stop::Again("GlobalProtect ESP probe entropy".into()))?;
        let mut transport = Self {
            send: esp::Send::new(keys.transmit),
            recv: esp::Receive::new(keys.receive),
            src,
            dst: keys.probe,
            id,
            probe: 0,
        };
        let mut input = [0; esp::MAX_WIRE + 1];
        for _ in 0..3 {
            transport
                .send_probe(socket)
                .map_err(|_| Stop::Again("GlobalProtect ESP probe send failed".into()))?;
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                ctl.fill(50)?;
                match socket.recv(&mut input) {
                    Ok(n) => {
                        if transport
                            .recv
                            .packet(&input[..n])
                            .is_ok_and(|p| transport.probe_reply(&p))
                        {
                            return Ok(transport);
                        }
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        Err(Stop::Again("GlobalProtect ESP probe timed out".into()))
    }

    fn send_probe(&mut self, socket: &UdpSocket) -> Result<(), ()> {
        self.probe = self.probe.wrapping_add(1);
        let mut packet = [0; 20 + 8 + PROBE_DATA.len()];
        let len = packet.len() as u16;
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&len.to_be_bytes());
        packet[8] = 64;
        packet[9] = 1;
        packet[12..16].copy_from_slice(&self.src.octets());
        packet[16..20].copy_from_slice(&self.dst.octets());
        packet[20] = 8;
        packet[24..26].copy_from_slice(&self.id);
        packet[26..28].copy_from_slice(&self.probe.to_be_bytes());
        packet[28..].copy_from_slice(PROBE_DATA);
        let ip_sum = checksum(&packet[..20]);
        let icmp_sum = checksum(&packet[20..]);
        packet[10..12].copy_from_slice(&ip_sum.to_be_bytes());
        packet[22..24].copy_from_slice(&icmp_sum.to_be_bytes());
        self.send_packet(socket, &packet)
    }

    fn probe_reply(&self, packet: &[u8]) -> bool {
        let header = usize::from(packet[0] & 15) * 4;
        packet[9] == 1
            && packet[12..16] == self.dst.octets()
            && packet[16..20] == self.src.octets()
            && packet.get(header..).is_some_and(|icmp| {
                icmp.len() == 8 + PROBE_DATA.len()
                    && icmp[..2] == [0, 0]
                    && icmp[4..6] == self.id
                    && icmp[6..8] == self.probe.to_be_bytes()
                    && &icmp[8..] == PROBE_DATA
                    && checksum(icmp) == 0
            })
    }

    fn send_packet(&mut self, socket: &UdpSocket, packet: &[u8]) -> Result<(), ()> {
        let wire = self.send.packet(packet).map_err(|_| ())?;
        let n = socket.send(&wire).map_err(|_| ())?;
        (n == wire.len()).then_some(()).ok_or(())
    }
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = bytes
        .chunks(2)
        .map(|p| u32::from(u16::from_be_bytes([p[0], *p.get(1).unwrap_or(&0)])))
        .sum::<u32>();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

impl Engine {
    /// Err means the UDP path failed and the next opener should use TLS.
    pub(super) fn gp_pump_esp(
        &mut self,
        mut transport: Transport,
        config: &gateway::Config,
    ) -> Result<Stop, ()> {
        let socket = self.esp.as_ref().ok_or(())?;
        let mut in_buf = [0; esp::MAX_WIRE + 1];
        let mut out_buf = [0; 4 + crypta_gp::ssl::MAX_PACKET + 1];
        let started = Instant::now();
        let (mut last_rx, mut last_probe, mut last_stats) = (started, started, started);
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.ctl.s.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.tun.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: all descriptors are live and owned by this engine.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 500) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(());
            }
            if fds[0].revents != 0 {
                if let Err(stop) = self.ctl.fill(0) {
                    return Ok(stop);
                }
            }
            if self
                .ctl
                .pending
                .drain(..)
                .any(|m| matches!(m, ToEngine::Rebind))
            {
                return Ok(Stop::Again("the network changed".into()));
            }
            if fds[1].revents != 0 {
                match socket.recv(&mut in_buf) {
                    Ok(n) => match transport.recv.packet(&in_buf[..n]) {
                        Ok(ip) => {
                            last_rx = Instant::now();
                            if !transport.probe_reply(&ip)
                                && ip.len() <= usize::from(config.push.mtu)
                            {
                                let mut frame = Vec::with_capacity(4 + ip.len());
                                frame.extend_from_slice(&AF_INET_HDR);
                                frame.extend_from_slice(&ip);
                                match self.tun.write(&frame) {
                                    Ok(n) if n == frame.len() => self.rx += ip.len() as u64,
                                    _ => self.dropped += 1,
                                }
                            }
                        }
                        Err(_) => self.dropped += 1,
                    },
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return Err(()),
                }
            }
            if fds[2].revents != 0 {
                match self.tun.read(&mut out_buf) {
                    Ok(0) => return Ok(Stop::Told),
                    Ok(n)
                        if n >= 24
                            && n <= 4 + usize::from(config.push.mtu)
                            && out_buf[..4] == AF_INET_HDR
                            && out_buf[16..20] == transport.src.octets() =>
                    {
                        transport.send_packet(socket, &out_buf[4..n])?;
                        self.tx += (n - 4) as u64;
                    }
                    Ok(_) => self.dropped += 1,
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Ok(Stop::Refused(format!("reading the tunnel: {e}"))),
                }
            }
            let now = Instant::now();
            if now.duration_since(started) >= config.renew_after {
                return Ok(Stop::Again("renewing the GlobalProtect tunnel".into()));
            }
            if now.duration_since(last_rx) >= DEAD {
                return Err(());
            }
            if now.duration_since(last_probe) >= INTERVAL {
                transport.send_probe(socket)?;
                last_probe = now;
            }
            if now.duration_since(last_stats) >= STATS_EVERY {
                self.ctl.send(&FromEngine::Stats {
                    rx: self.rx,
                    tx: self.tx,
                    dropped: self.dropped,
                });
                last_stats = now;
            }
        }
    }
}
