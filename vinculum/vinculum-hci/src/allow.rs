//! The command allowlist (spec §5.3).
//!
//! B1 rule: a command passes if its opcode is a standard HCI command known
//! to the generated grammar (OGF 0x01–0x08) and is not on [`DENY`].
//! Vendor commands (OGF 0x3F) are refused, except a per-vendor list that
//! cannot write controller memory (empty until the firmware loaders land:
//! firmware download happens inside the transport, before the host is
//! connected, so the host never needs them). The list narrows to exactly the
//! commands `vinculumd` issues once its command set settles (B4).

use vinculum_hci_proto::hci::OpCode;

pub const OGF_VENDOR: u16 = 0x3F;

/// Standard commands the host never needs and that change controller state
/// in ways a compromised host could abuse.
pub const DENY: &[u16] = &[
    0x1802, // Write_Loopback_Mode: would loop the host's own packets back
    0x1804, // Write_Simple_Pairing_Debug_Mode: makes pairing traffic decryptable
    0x1805, // Enable_Device_Under_Test_Mode: hands the radio to a tester
];

/// Vendor commands allowed after bring-up, per vendor. None yet.
pub const VENDOR_ALLOWED: &[u16] = &[];

pub fn allowed(opcode: u16) -> bool {
    let ogf = opcode >> 10;
    if ogf == OGF_VENDOR {
        return VENDOR_ALLOWED.contains(&opcode);
    }
    if !(0x01..=0x08).contains(&ogf) {
        return false;
    }
    if DENY.contains(&opcode) {
        return false;
    }
    matches!(OpCode::try_from(opcode), Ok(op) if op != OpCode::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert!(allowed(0x0C03), "Reset");
        assert!(allowed(0x200B), "LE Set Scan Parameters");
        assert!(allowed(0x2027), "LE Add Device To Resolving List");
        assert!(!allowed(0xFC01), "vendor");
        assert!(!allowed(0xFC4F), "vendor");
        assert!(!allowed(0x1804), "SSP debug mode");
        assert!(!allowed(0x1805), "DUT mode");
        assert!(!allowed(0x0000), "NOP");
        assert!(!allowed(0x2FFF), "unknown LE opcode");
        assert!(!allowed(0x2400), "OGF 9");
    }
}
