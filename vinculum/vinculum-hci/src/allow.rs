//! The command allowlist (spec §5.3).
//!
//! B1 rule: a command passes if its opcode is a standard HCI command known
//! to the generated grammar (OGF 0x01–0x08) and is not on [`DENY`].
//! Vendor commands (OGF 0x3F) are refused, except a per-vendor list that
//! cannot write controller memory. The vendor is the manufacturer the
//! controller itself reported in Read Local Version (the relay watches for
//! it), so the same opcode on another vendor's chip stays refused. Firmware
//! download happens inside the transport before any host connects, so the
//! host never needs a loader's commands. The list narrows to exactly the
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

/// Vendor commands allowed after bring-up, as (manufacturer, opcode).
pub const VENDOR_ALLOWED: &[(u16, u16)] = &[
    // Zephyr (Linux Foundation, 0x05F1): Read Static Addresses, read-only;
    // the nRF52840 dongle has no public address without it.
    (0x05F1, 0xFC09),
];

/// Whether the host may send `opcode` to a controller that reported
/// `manufacturer` (None: not reported yet).
pub fn allowed(opcode: u16, manufacturer: Option<u16>) -> bool {
    let ogf = opcode >> 10;
    if ogf == OGF_VENDOR {
        return manufacturer.is_some_and(|m| VENDOR_ALLOWED.contains(&(m, opcode)));
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
        let rtk = Some(0x005D);
        assert!(allowed(0x0C03, None), "Reset");
        assert!(allowed(0x200B, rtk), "LE Set Scan Parameters");
        assert!(allowed(0x2027, rtk), "LE Add Device To Resolving List");
        assert!(!allowed(0xFC01, rtk), "vendor");
        assert!(!allowed(0xFC4F, rtk), "vendor");
        assert!(!allowed(0x1804, rtk), "SSP debug mode");
        assert!(!allowed(0x1805, rtk), "DUT mode");
        assert!(!allowed(0x0000, rtk), "NOP");
        assert!(!allowed(0x2FFF, rtk), "unknown LE opcode");
        // Zephyr's Read Static Addresses: only on a Zephyr controller.
        assert!(allowed(0xFC09, Some(0x05F1)));
        assert!(!allowed(0xFC09, rtk));
        assert!(!allowed(0xFC09, None));
        assert!(!allowed(0x2400, rtk), "OGF 9");
    }
}
