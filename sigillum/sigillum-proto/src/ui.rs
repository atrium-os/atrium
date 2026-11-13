//! `sigillumd` <-> `sigillum-ui`, the trusted sheet (sigillum.md §7.2,
//! §7.3), on a socket only `_sigillum-ui` may connect to. The UI draws what
//! it is told; the broker decides. The UI spends the press token with
//! frescod (purpose `IDENTITY`) itself and reports whether frescod accepted
//! it, as arca-ui does for its fill chip.

#[allow(unused_imports)]
use alloc::{boxed::Box, string::String, vec::Vec};
use serde::{Deserialize, Serialize};

use crate::client::{CertPurpose, Purpose};
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
    /// Certificates on the attached tokens (§10.1). `sign`: this is the
    /// signature itself (one certificate, a PIN follows); otherwise the user
    /// picks which certificate the server gets.
    Certificates { requester: String, purpose: CertPurpose, sign: bool, certs: Vec<CertView> },
}

/// Most certificates one sheet lists.
pub const MAX_CERTS: usize = 16;

/// A certificate as the sheet lists it: read by sigillumd from the
/// certificate itself, never the token's label for it (except `token`, the
/// token's own name for itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertView {
    pub id: u64,
    pub subject: String,
    pub issuer: String,
    /// `YYYY-MM-DD`.
    pub not_after: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    Device(u64),
    /// Register: save the passkey in Arca.
    Platform,
    /// Assert: this Arca passkey (its credential id).
    Passkey([u8; 16]),
    Passcode,
    /// A certificate on the sheet (its [`CertView::id`]).
    Certificate(u64),
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
    /// `retries`: tries left, `255` when the device doesn't say; `wrong`:
    /// the last PIN typed was wrong.
    AskPin { key: SheetKey, device: u64, retries: u8, wrong: bool },
    Touch { key: SheetKey, device: u64 },
    /// The user chose "use passcode" on a verify sheet: take it in secure
    /// input and send it back as [`FromUi::Passcode`].
    /// `wrong`: the last passcode typed was wrong.
    AskPasscode { key: SheetKey, wrong: bool },
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
