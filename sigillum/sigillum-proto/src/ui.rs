//! `sigillumd` <-> `sigillum-ui`, the trusted sheet (sigillum.md §7.2,
//! §7.3), on a socket only `_sigillum-ui` may connect to. The UI draws what
//! it is told; the broker decides. The UI spends the press token with
//! frescod (purpose `IDENTITY`) itself and reports whether frescod accepted
//! it, as arca-ui does for its fill chip.

use serde::{Deserialize, Serialize};

use crate::client::Purpose;
use crate::driver::Pin;

pub const MAX_MESSAGE: usize = 4096;
pub const SOCKET: &str = "/atrium/sockets/sigillum-ui/ui.sock";

/// Which ceremony a sheet belongs to: the broker's own key.
pub type SheetKey = u64;

/// What the sheet says. `requester` comes from the kernel-verified label,
/// `rp_id` is what the ceremony is bound to; neither is page text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sheet {
    Register { requester: String, rp_id: String, user_name: String },
    Assert { requester: String, rp_id: String },
    Verify { requester: String, user: String, purpose: Purpose },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    Device(u64),
    Passcode,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: u64,
    pub name: String,
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
