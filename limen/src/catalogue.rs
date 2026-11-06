//! The role catalogue (limen.md §2.0).

/// What frescod does with a slot (limen.md §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SlotKind {
    /// Pinned and clipped to the parent.
    Plain,
    /// Pinned, plus the visibility record, `FILL` presses and the frame
    /// (arca-autofill.md §4).
    Credential,
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
}

pub const ARCA: &str = "org.atrium.arca";
pub const VESTIBULUM: &str = "org.atrium.vestibulum";

pub const ROLES: &[Role] = &[
    Role { name: "credential", provider: Provider::System(ARCA), slot: SlotKind::Credential },
    Role { name: "sign-in", provider: Provider::System(VESTIBULUM), slot: SlotKind::Credential },
    Role { name: "doc-viewer", provider: Provider::User, slot: SlotKind::Plain },
    Role { name: "picker", provider: Provider::User, slot: SlotKind::Plain },
    Role { name: "share-target", provider: Provider::User, slot: SlotKind::Plain },
    Role { name: "media-player", provider: Provider::User, slot: SlotKind::Plain },
    Role { name: "payment", provider: Provider::User, slot: SlotKind::Credential },
    Role { name: "map", provider: Provider::User, slot: SlotKind::Plain },
    Role { name: "editor-extension", provider: Provider::User, slot: SlotKind::Plain },
];

pub fn role(name: &str) -> Option<&'static Role> {
    ROLES.iter().find(|r| r.name == name)
}

/// Limits (limen.md §7).
pub const MAX_EMBEDS_PER_PARENT: usize = 16;
pub const MAX_MESSAGE: usize = 64 * 1024;
pub const MAX_MESSAGES_PER_SEC: u32 = 256;
/// How long a child has between `Detaching` and the end of its jail.
pub const DETACH_GRACE_MS: u64 = 500;
