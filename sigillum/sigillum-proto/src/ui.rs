//! `sigillumd` <-> `sigillum-ui`, the trusted sheet (sigillum.md §7.2,
//! §7.3), on a socket only `_sigillum-ui` may connect to. The UI draws what
//! it is told; the broker decides. The UI spends the press token with
//! frescod (purpose `IDENTITY`) itself and reports whether frescod accepted
//! it, as arca-ui does for its fill chip.

use serde::{Deserialize, Serialize};

use crate::client::Purpose;
use crate::driver::Pin;

/// Room for a sheet with [`MAX_PASSKEYS`] passkeys and every device.
pub const MAX_MESSAGE: usize = 16 * 1024;
/// Most of the user's passkeys one sheet lists.
pub const MAX_PASSKEYS: usize = 16;
pub const SOCKET: &str = "/atrium/sockets/sigillum-ui/ui.sock";

/// Which ceremony a sheet belongs to: the broker's own key.
pub type SheetKey = u64;

/// What the sheet says. `requester` comes from the kernel-verified label,
/// `rp_id` is what the ceremony is bound to; neither is page text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sheet {
    /// `platform`: "Save a passkey on this device" (Arca) is offered.
    Register { requester: String, rp_id: String, user_name: String, platform: bool },
    /// `passkeys`: the user's Arca passkeys for this RP that the request
    /// allows, offered before security keys.
    Assert { requester: String, rp_id: String, passkeys: Vec<PasskeyView> },
    Verify { requester: String, user: String, purpose: Purpose },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    Device(u64),
    /// Register: save the passkey in Arca.
    Platform,
    /// Assert: this Arca passkey (its credential id).
    Passkey([u8; 16]),
    Passcode,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: u64,
    pub name: String,
}

/// One of the user's Arca passkeys, as the sheet lists it. The names are
/// the ones the site gave at registration: shown as the site's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyView {
    pub id: [u8; 16],
    pub name: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToUi {
    Show { key: SheetKey, sheet: Sheet, devices: Vec<DeviceView> },
    AskPin { key: SheetKey, device: u64, retries: u8 },
    Touch { key: SheetKey, device: u64 },
    /// The user chose "use passcode" on a verify sheet: take it in secure
    /// input and send it back as [`FromUi::Passcode`].
    AskPasscode { key: SheetKey },
    Close { key: SheetKey },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromUi {
    /// A press on the sheet; `activation_ok` is frescod's verdict on the
    /// press token.
    Press { key: SheetKey, choice: Choice, activation_ok: bool },
    Pin { key: SheetKey, pin: Pin },
    /// The passcode typed on a verify sheet, for ostiarius to check.
    Passcode { key: SheetKey, passcode: Pin },
}
