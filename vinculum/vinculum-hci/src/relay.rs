//! The relay between controller and host, sans-I/O.
//!
//! Controller to host: every well-formed frame except commands (a controller
//! never sends one). Host to controller: commands that pass the allowlist,
//! and data (ACL, SCO, ISO). A refused command is answered to the host with
//! a Command Status of Unknown HCI Command, so the host's flow control keeps
//! going. A malformed stream from either side is an error: the caller drops
//! that side (and resets the controller if it was the host).

use crate::allow;
use vinculum_hci_proto::h4::{self, Decoder, Frame, Kind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    ToController(Vec<u8>),
    ToHost(Vec<u8>),
}

#[derive(Debug, Default)]
pub struct Relay {
    from_controller: Decoder,
    from_host: Decoder,
    pub refused: u32,
    pub dropped: u32,
}

/// The H4 bytes of a Command Status event refusing `opcode`.
pub fn refusal(opcode: u16) -> Vec<u8> {
    let op = opcode.to_le_bytes();
    // event 0x0F, length 4: status 0x01 Unknown HCI Command, 1 credit, opcode
    vec![0x04, 0x0F, 0x04, 0x01, 0x01, op[0], op[1]]
}

impl Relay {
    pub fn new() -> Relay {
        Relay::default()
    }

    /// A new host connected: start its stream clean.
    pub fn host_reset(&mut self) {
        self.from_host.reset();
    }

    pub fn from_controller(&mut self, data: &[u8], out: &mut Vec<Out>) -> Result<(), h4::Error> {
        let mut frames = Vec::new();
        self.from_controller.push(data, |f| frames.push(f))?;
        for f in frames {
            if f.kind == Kind::Command {
                self.dropped += 1;
                continue;
            }
            out.push(Out::ToHost(f.to_bytes()));
        }
        Ok(())
    }

    pub fn from_host(&mut self, data: &[u8], out: &mut Vec<Out>) -> Result<(), h4::Error> {
        let mut frames: Vec<Frame> = Vec::new();
        self.from_host.push(data, |f| frames.push(f))?;
        for f in frames {
            match f.kind {
                Kind::Command => {
                    let opcode = u16::from_le_bytes([f.packet[0], f.packet[1]]);
                    if allow::allowed(opcode) {
                        out.push(Out::ToController(f.to_bytes()));
                    } else {
                        self.refused += 1;
                        out.push(Out::ToHost(refusal(opcode)));
                    }
                }
                Kind::Acl | Kind::Sco | Kind::Iso => out.push(Out::ToController(f.to_bytes())),
                Kind::Event => self.dropped += 1,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vinculum_hci_proto::hci::{Event, EventChild};
    use vinculum_hci_proto::Packet;

    #[test]
    fn refused_command_answered_with_command_status() {
        let mut r = Relay::new();
        let mut out = Vec::new();
        // Enable_Device_Under_Test_Mode (0x1805), denied
        r.from_host(&[0x01, 0x05, 0x18, 0x00], &mut out).unwrap();
        // a vendor command (Intel-style 0xFC0E) with a payload
        r.from_host(&[0x01, 0x0E, 0xFC, 0x02, 0xAA, 0xBB], &mut out).unwrap();
        assert_eq!(r.refused, 2);
        let Out::ToHost(ev) = &out[0] else { panic!() };
        let e = Event::decode_full(&ev[1..]).unwrap();
        let EventChild::CommandStatus(cs) = e.specialize().unwrap() else { panic!() };
        assert_eq!(cs.status, vinculum_hci_proto::hci::ErrorCode::UnknownHciCommand);
        assert_eq!(u16::from(cs.command_op_code), 0x1805);
        assert_eq!(out[1], Out::ToHost(refusal(0xFC0E)));
    }

    #[test]
    fn standard_traffic_passes_both_ways() {
        let mut r = Relay::new();
        let mut out = Vec::new();
        r.from_host(&[0x01, 0x03, 0x0C, 0x00], &mut out).unwrap(); // Reset
        r.from_host(&[0x02, 0x40, 0x00, 0x01, 0x00, 0x99], &mut out).unwrap(); // ACL
        r.from_controller(&[0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00], &mut out).unwrap();
        assert!(matches!(out[0], Out::ToController(_)));
        assert!(matches!(out[1], Out::ToController(_)));
        assert!(matches!(out[2], Out::ToHost(_)));
        assert_eq!(r.refused, 0);
    }

    #[test]
    fn wrong_direction_frames_dropped() {
        let mut r = Relay::new();
        let mut out = Vec::new();
        r.from_controller(&[0x01, 0x03, 0x0C, 0x00], &mut out).unwrap(); // a "command" from the controller
        r.from_host(&[0x04, 0x0E, 0x00], &mut out).unwrap(); // an "event" from the host
        assert!(out.is_empty());
        assert_eq!(r.dropped, 2);
    }
}
