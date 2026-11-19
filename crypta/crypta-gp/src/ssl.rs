//! Packet framing after the gateway's START_TUNNEL marker.
//!
//! Wire format documented in PAN_GlobalProtect_protocol_doc.md at
//! https://github.com/dlenski/openconnect/blob/master/PAN_GlobalProtect_protocol_doc.md

const MAGIC: [u8; 4] = [0x1a, 0x2b, 0x3c, 0x4d];
const HEADER_LEN: usize = 16;
const IPV4: u16 = 0x0800;
const IPV6: u16 = 0x86dd;

/// Current maximum tunnel MTU from vpn.md §5.2.
pub const MAX_PACKET: usize = 1500;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Keepalive,
    Packet(&'a [u8]),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Header,
    Length,
    Packet,
}

fn packet_type(packet: &[u8]) -> Result<u16, Error> {
    if packet.len() > MAX_PACKET {
        return Err(Error::Length);
    }
    match packet.first().map(|byte| byte >> 4) {
        Some(4) if packet.len() >= 20 => {
            let header_len = usize::from(packet[0] & 0xf) * 4;
            let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
            if header_len >= 20 && header_len <= total_len && total_len == packet.len() {
                return Ok(IPV4);
            }
        }
        Some(6) if packet.len() >= 40 => {
            let payload_len = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
            if 40 + payload_len == packet.len() {
                return Ok(IPV6);
            }
        }
        _ => {}
    }
    Err(Error::Packet)
}

/// Decode one frame and return the unused suffix. On incomplete input the
/// caller retains the prefix and reads up to HEADER_LEN + MAX_PACKET bytes.
/// An invalid frame terminates the stream.
pub fn decode(input: &[u8]) -> Result<Option<(Frame<'_>, &[u8])>, Error> {
    let Some(header) = input.get(..HEADER_LEN) else {
        return Ok(None);
    };
    if header[..4] != MAGIC || header[9..] != [0; 7] {
        return Err(Error::Header);
    }
    let kind = u16::from_be_bytes([header[4], header[5]]);
    let len = usize::from(u16::from_be_bytes([header[6], header[7]]));
    if len == 0 && kind == 0 && header[8] == 0 {
        return Ok(Some((Frame::Keepalive, &input[HEADER_LEN..])));
    }
    if !matches!(kind, IPV4 | IPV6) || header[8] != 1 {
        return Err(Error::Header);
    }
    if !(20..=MAX_PACKET).contains(&len) || (kind == IPV6 && len < 40) {
        return Err(Error::Length);
    }
    let end = HEADER_LEN + len;
    let Some(packet) = input.get(HEADER_LEN..end) else {
        return Ok(None);
    };
    if packet_type(packet)? != kind {
        return Err(Error::Packet);
    }
    Ok(Some((Frame::Packet(packet), &input[end..])))
}

pub fn encode(frame: Frame<'_>) -> Result<Vec<u8>, Error> {
    let (kind, packet, flag) = match frame {
        Frame::Keepalive => (0, &[][..], 0),
        Frame::Packet(packet) => (packet_type(packet)?, packet, 1),
    };
    let mut header = [0; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&kind.to_be_bytes());
    header[6..8].copy_from_slice(&(packet.len() as u16).to_be_bytes());
    header[8] = flag;
    let mut output = Vec::with_capacity(HEADER_LEN + packet.len());
    output.extend_from_slice(&header);
    output.extend_from_slice(packet);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal headers exercise the wire layout independently of the encoder.
    const IPV4_HEADER: [u8; 16] = [26, 43, 60, 77, 8, 0, 0, 20, 1, 0, 0, 0, 0, 0, 0, 0];
    const KEEPALIVE: [u8; 16] = [26, 43, 60, 77, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    const PACKET: [u8; 20] = [
        0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
    ];

    #[test]
    fn reads_split_and_coalesced_frames() {
        let wire = [&IPV4_HEADER[..], &PACKET, &KEEPALIVE].concat();
        for split in 0..HEADER_LEN + PACKET.len() {
            assert_eq!(decode(&wire[..split]), Ok(None));
        }
        let (frame, tail) = decode(&wire).unwrap().unwrap();
        assert_eq!(frame, Frame::Packet(&PACKET));
        assert_eq!(decode(tail), Ok(Some((Frame::Keepalive, &[][..]))));
        assert_eq!(encode(frame).unwrap(), &wire[..HEADER_LEN + PACKET.len()]);
        assert_eq!(encode(Frame::Keepalive).unwrap(), KEEPALIVE);
    }

    #[test]
    fn rejects_bad_headers_and_packets() {
        let mut header = IPV4_HEADER;
        header[6..8].copy_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(decode(&header), Err(Error::Length));
        header = IPV4_HEADER;
        header[9] = 1;
        assert_eq!(decode(&header), Err(Error::Header));
        let mut packet = PACKET;
        packet[3] -= 1;
        assert_eq!(
            decode(&[&IPV4_HEADER[..], &packet].concat()),
            Err(Error::Packet)
        );
        assert_eq!(encode(Frame::Packet(&packet)), Err(Error::Packet));
    }

    #[test]
    fn checks_ipv6_length_and_ethertype() {
        let mut packet = [0; 41];
        packet[0] = 0x60;
        packet[5] = 1;
        let wire = encode(Frame::Packet(&packet)).unwrap();
        assert_eq!(wire[4..8], [0x86, 0xdd, 0, 41]);
        assert_eq!(decode(&wire), Ok(Some((Frame::Packet(&packet), &[][..]))));
        let mut wire = wire;
        wire[4..6].copy_from_slice(&IPV4.to_be_bytes());
        assert_eq!(decode(&wire), Err(Error::Packet));
    }
}
