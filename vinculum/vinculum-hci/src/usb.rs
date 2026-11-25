//! The USB transport's pure part (Bluetooth Core 5.4, Vol 4, Part B): which
//! interface and endpoints carry HCI, and where each frame from the host goes.
//! The libusb calls are in the binary (`src/usb_dev.rs`).
//!
//! On USB, HCI has no H4 type bytes: the pipe says the kind. Commands go out
//! as class requests on the default control pipe, events come in on the
//! interrupt IN endpoint, ACL data goes out and in on the bulk pair. SCO uses
//! the isochronous endpoints of interface 1 and is not carried yet (no audio
//! before B5); ISO data waits for LE Audio (B7).

use vinculum_hci_proto::h4::{Frame, Kind};

/// Interface class/subclass/protocol of a primary Bluetooth controller.
pub const CLASS_WIRELESS: u8 = 0xE0;
pub const SUBCLASS_RF: u8 = 0x01;
pub const PROTOCOL_BLUETOOTH: u8 = 0x01;

/// bmRequestType of an HCI command: host to device, class, recipient device
/// (Core Vol 4 Part B §2.2.2; Zephyr and Realtek controllers also accept
/// recipient interface, but device is what the spec names).
pub const COMMAND_REQUEST_TYPE: u8 = 0x20;

pub fn is_bluetooth(class: u8, subclass: u8, protocol: u8) -> bool {
    class == CLASS_WIRELESS && subclass == SUBCLASS_RF && protocol == PROTOCOL_BLUETOOTH
}

/// One endpoint as its descriptor gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    pub max_packet: u16,
}

impl Endpoint {
    fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
    fn transfer_type(&self) -> u8 {
        self.attributes & 0x03
    }
    /// Payload bytes per USB packet (bits 0..10; bits 11..12 are extra
    /// transactions per microframe, which only isochronous and interrupt
    /// high-bandwidth endpoints use).
    pub fn packet_size(&self) -> usize {
        (self.max_packet & 0x07FF) as usize
    }
}

/// The three endpoints of the HCI interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HciEndpoints {
    pub events: Endpoint,
    pub acl_in: Endpoint,
    pub acl_out: Endpoint,
}

/// Picks the interrupt IN, bulk IN and bulk OUT endpoints from interface 0's
/// alternate setting 0. `None` if any is missing or has a zero packet size,
/// which a controller we can drive never has.
pub fn pick_endpoints(eps: &[Endpoint]) -> Option<HciEndpoints> {
    let find = |tt: u8, input: bool| eps.iter().copied().find(|e| e.transfer_type() == tt && e.is_in() == input && e.packet_size() > 0);
    Some(HciEndpoints { events: find(3, true)?, acl_in: find(2, true)?, acl_out: find(2, false)? })
}

/// Where one frame from the host goes on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Class request on the control pipe; the data is the command packet.
    Control(Vec<u8>),
    /// Bulk OUT; the data is the ACL packet.
    Bulk(Vec<u8>),
    /// Not carried by this transport yet (SCO, ISO), or never sent by a host
    /// (events).
    Unsupported(Kind),
}

pub fn route(f: Frame) -> Route {
    match f.kind {
        Kind::Command => Route::Control(f.packet),
        Kind::Acl => Route::Bulk(f.packet),
        k => Route::Unsupported(k),
    }
}

/// Which controller a `usb:` spec names: the first Bluetooth controller, or
/// one by vendor and product id (`usb:2357:0604`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    First,
    Ids { vendor: u16, product: u16 },
}

pub fn parse_select(rest: &str) -> Option<Select> {
    if rest.is_empty() {
        return Some(Select::First);
    }
    let (v, p) = rest.split_once(':')?;
    if v.len() != 4 || p.len() != 4 {
        return None;
    }
    Some(Select::Ids { vendor: u16::from_str_radix(v, 16).ok()?, product: u16::from_str_radix(p, 16).ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(address: u8, attributes: u8, max_packet: u16) -> Endpoint {
        Endpoint { address, attributes, max_packet }
    }

    #[test]
    fn endpoints_of_a_zephyr_hci_usb_interface() {
        // Zephyr's hci_usb: interrupt IN 0x81 (16), bulk IN 0x82, bulk OUT 0x01.
        let e = pick_endpoints(&[ep(0x81, 0x03, 16), ep(0x82, 0x02, 64), ep(0x01, 0x02, 64)]).unwrap();
        assert_eq!(e.events.address, 0x81);
        assert_eq!(e.acl_in.address, 0x82);
        assert_eq!(e.acl_out.address, 0x01);
        assert_eq!(e.events.packet_size(), 16);
    }

    #[test]
    fn missing_or_empty_endpoint_refused() {
        assert_eq!(pick_endpoints(&[ep(0x81, 0x03, 16), ep(0x82, 0x02, 64)]), None);
        assert_eq!(pick_endpoints(&[ep(0x81, 0x03, 0), ep(0x82, 0x02, 64), ep(0x02, 0x02, 64)]), None);
        // An isochronous IN is not the event endpoint.
        assert_eq!(pick_endpoints(&[ep(0x83, 0x01, 49), ep(0x82, 0x02, 64), ep(0x02, 0x02, 64)]), None);
    }

    #[test]
    fn high_bandwidth_bits_ignored() {
        assert_eq!(ep(0x82, 0x02, 0x1200).packet_size(), 0x200);
    }

    #[test]
    fn frames_route_by_kind() {
        let reset = Frame::parse(&[0x01, 0x03, 0x0C, 0x00]).unwrap();
        assert_eq!(route(reset), Route::Control(vec![0x03, 0x0C, 0x00]));
        let acl = Frame::parse(&[0x02, 0x01, 0x20, 0x01, 0x00, 0xAA]).unwrap();
        assert_eq!(route(acl), Route::Bulk(vec![0x01, 0x20, 0x01, 0x00, 0xAA]));
        let sco = Frame::parse(&[0x03, 0x01, 0x00, 0x00]).unwrap();
        assert_eq!(route(sco), Route::Unsupported(Kind::Sco));
    }

    #[test]
    fn select_specs() {
        assert_eq!(parse_select(""), Some(Select::First));
        assert_eq!(parse_select("2357:0604"), Some(Select::Ids { vendor: 0x2357, product: 0x0604 }));
        assert_eq!(parse_select("2357"), None);
        assert_eq!(parse_select("2357:60"), None);
        assert_eq!(parse_select("zzzz:0604"), None);
    }

    #[test]
    fn bluetooth_class() {
        assert!(is_bluetooth(0xE0, 0x01, 0x01));
        assert!(!is_bluetooth(0xE0, 0x01, 0x04)); // RNDIS
        assert!(!is_bluetooth(0xFF, 0x01, 0x01));
    }
}
