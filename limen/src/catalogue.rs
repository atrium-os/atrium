//! The role catalogue (limen.md §2.0).

/// What frescod does with a slot (limen.md §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SlotKind {
    /// Pinned and clipped to the parent.
    Plain,
    /// Pinned, plus the visibility record, `FILL` presses and the frame
    /// (arca-autofill.md §4).
    Credential,
    /// Live code in a page (navigator-ui.md §6.3): frescod's frame and
    /// origin tag, no input while anything overlaps it.
    Island,
    /// A system dialog over the parent (scrinium.md §4.4): frescod places
    /// it, `w` x `h`, centred near the parent's top whatever rect the
    /// parent asked for, dims the parent and keeps its input from it, and
    /// frames it with a tag the parent can't draw over.
    Sheet { w: u16, h: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// A system service, by registry app id. Never chosen, never registered for.
    System(&'static str),
    /// Apps whose manifest declares the role; the user's default.
    User,
}

#[derive(Debug, Clone, Copy)]
pub struct Role {
    pub name: &'static str,
    pub provider: Provider,
    pub slot: SlotKind,
    /// The parent may pass one descriptor with a message (limen.md §2.2):
    /// a read-only file or pipe, checked and narrowed by limend.
    pub takes_fd: bool,
    /// The child may pass one descriptor back with a message (`EMIT_FD`):
    /// a regular file, read-only or read-write, checked and narrowed by
    /// limend. The picker's picked file (limen.md §2.3).
    pub returns_fd: bool,
}

pub const ARCA: &str = "org.atrium.arca";
pub const VESTIBULUM: &str = "org.atrium.vestibulum";
/// The Navigator's engine jail host (legacy-web tier 4): a page's live
/// islands.
pub const NAVIGATOR_ENGINE: &str = "org.atrium.navigator.engine";
/// Scrinium, the file manager, which also serves the system picker
/// (scrinium.md §4): it holds the user's sticks and hands out one pick.
pub const SCRINIUM: &str = "org.atrium.scrinium";

pub const ROLES: &[Role] = &[
    Role { name: "credential", provider: Provider::System(ARCA), slot: SlotKind::Credential, takes_fd: false, returns_fd: false },
    Role { name: "sign-in", provider: Provider::System(VESTIBULUM), slot: SlotKind::Credential, takes_fd: false, returns_fd: false },
    Role { name: "doc-viewer", provider: Provider::User, slot: SlotKind::Plain, takes_fd: true, returns_fd: false },
    Role { name: "picker", provider: Provider::System(SCRINIUM), slot: SlotKind::Sheet { w: 640, h: 440 }, takes_fd: false, returns_fd: true },
    Role { name: "share-target", provider: Provider::User, slot: SlotKind::Plain, takes_fd: false, returns_fd: false },
    Role { name: "media-player", provider: Provider::User, slot: SlotKind::Plain, takes_fd: true, returns_fd: false },
    Role { name: "payment", provider: Provider::User, slot: SlotKind::Credential, takes_fd: false, returns_fd: false },
    Role { name: "web-island", provider: Provider::System(NAVIGATOR_ENGINE), slot: SlotKind::Island, takes_fd: false, returns_fd: false },
    Role { name: "map", provider: Provider::User, slot: SlotKind::Plain, takes_fd: false, returns_fd: false },
    Role { name: "editor-extension", provider: Provider::User, slot: SlotKind::Plain, takes_fd: false, returns_fd: false },
];

/// Roles whose children may play sound as their parent (lyra-v1.md §4):
/// limend asks choragusd for an attribution ticket for each embed.
pub const AUDIO_ROLES: &[&str] = &["media-player"];

/// Roles whose child may go picture-in-picture (atrium-player-pip-keys.md
/// §3): its window leaves the slot for Forum's `pip` layer.
pub const PIP_ROLES: &[&str] = &["media-player"];
/// A PiP request needs a press routed to the child's or the parent's
/// window within this long (§3.1 step 3).
pub const PIP_GESTURE_MS: u32 = 1000;

pub fn role(name: &str) -> Option<&'static Role> {
    ROLES.iter().find(|r| r.name == name)
}

/// Limits (limen.md §7).
pub const MAX_EMBEDS_PER_PARENT: usize = 16;
pub const MAX_MESSAGE: usize = 64 * 1024;
pub const MAX_MESSAGES_PER_SEC: u32 = 256;
/// How long a child has between `Detaching` and the end of its jail.
pub const DETACH_GRACE_MS: u64 = 500;
