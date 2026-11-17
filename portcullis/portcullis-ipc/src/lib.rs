//! portcullis-ipc — wire protocol between `portcullis` CLI and
//! `portcullisd`. Newline-delimited JSON over a Unix-domain socket
//! at `SOCKET_PATH`. Each line is one `Request` from the client or
//! one `Response` from the daemon; framing is "until the next \n".
//!
//! JSON (vs bincode/protobuf) chosen so the protocol is debuggable
//! with `nc -U` and unimportant from a perf perspective — request
//! rate is "human-driven app launches", not anything hot.
//!
//! Versioning: every `Request` and `Response` is tagged with its
//! variant name via `#[serde(tag = "op")]` / `#[serde(tag = "kind")]`,
//! so adding new variants is forward-compatible (older daemons reply
//! `UnknownOp`; older clients ignore unknown response kinds).

use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use portcullis_toml::Capabilities;

pub mod fdpass;
pub use fdpass::{recv_fds, send_fds, send_fds_with};

/// Default socket path. Created by portcullisd at startup mode 0666
/// (world-connectable); per-user authorization happens inside the
/// daemon via getpeereid(2). Lives under /atrium/sockets/ so
/// atrium-session's bind-mount of that directory exposes it to
/// processes inside session jails without an extra mount entry.
pub const SOCKET_PATH: &str = "/atrium/sockets/portcullis.sock";

/// The same socket under the per-service DIRECTORY convention:
/// `/atrium/sockets/<service>/<service>.sock`.
///
/// This exists because a jail cannot be given the flat path above.
/// `mount_nullfs` refuses to mount a unix-socket node ("must be either a file or
/// directory"), so every capability-granted socket is reached by mounting the
/// service's own directory — see portcullis-jail's `apply_socket`. With the
/// socket sitting loose in the shared sockets root there is no directory to
/// mount that wouldn't also expose every OTHER service's socket, which is
/// exactly the capability granularity that design protects.
///
/// portcullisd binds HERE and leaves the flat path as a symlink, so existing
/// clients (the CLI, anything holding `SOCKET_PATH`) keep working unchanged.
pub const SERVICE_SOCKET_PATH: &str = "/atrium/sockets/portcullis/portcullis.sock";

/// Where a client should look for portcullisd: the per-service path first (the
/// only one that exists inside a jail), then the flat compat path.
///
/// Returning a path that exists — rather than one that merely might — keeps the
/// in-jail failure honest: if neither is present the caller reports "not
/// reachable" instead of a confusing ENOENT on a path that was never the right
/// one for its context.
pub fn resolve_socket_path() -> Option<&'static str> {
    [SERVICE_SOCKET_PATH, SOCKET_PATH]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

/// Protocol version — bumped if a wire-incompatible change lands.
/// Clients send their version in `Request::Hello`; mismatched versions
/// get `Response::ProtoMismatch` and the daemon closes the connection.
pub const PROTO_VERSION: u32 = 1;

/// The fetcher `OpenFetcher` starts (docs/spec/nuntius.md).
pub const NUNTIUS_APP: &str = "org.atrium.nuntius";

/// The content decryption module host `OpenCdm` starts (docs/spec/hospes.md).
pub const HOSPES_CDM_APP: &str = "org.atrium.hospes-cdm";

/// The bounded reclaim signal a memory governor may request — mirrors jaild's
/// `ReapSignal` but keeps this app-protocol crate free of a jaild dependency
/// (portcullisd maps one to the other). No raw signal numbers cross the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovSignal {
    /// SIGINFO — shed caches, keep running.
    Trim,
    /// SIGTERM — exit gracefully.
    Exit,
    /// SIGKILL — force.
    Kill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Handshake; must be the first message on a fresh connection.
    Hello { version: u32 },
    /// Cheap liveness probe.
    Ping,
    /// "Can app `app_id` (with this manifest hash and these caps)
    /// launch right now?" Daemon returns `Authorized` if the policy
    /// already covers everything, `NeedsApproval` with the delta
    /// otherwise.
    Authorize {
        app_id:        String,
        manifest_hash: String,
        requested:     Capabilities,
    },
    /// "Persist a grant for these capabilities." Used by both the
    /// CLI bootstrap (`policy grant`) and, in the future, the
    /// prompt UI after the user clicks Allow.
    Grant {
        app_id:        String,
        manifest_hash: String,
        capabilities:  Capabilities,
    },
    /// Drop a previously persisted grant.
    Revoke { app_id: String },
    /// Re-read policy.toml from disk (for after manual edits).
    Reload,
    /// Run an installed app inside its per-app jail. Daemon does
    /// the policy check, the overlay mount, the jail(8) invocation,
    /// and the teardown — CLI just forwards and waits for the exit
    /// status. `bypass_policy = true` is the daemon-side equivalent
    /// of `portcullis launch --no-prompt` (dev mode; nothing
    /// persisted).
    ///
    /// Stdio: this commit (Phase 4.4 step 1) inherits the daemon's
    /// stdio, so app output lands in the daemon log. SCM_RIGHTS pty
    /// passing arrives in step 2 and replaces this with proper
    /// terminal handoff to the requesting client.
    Launch {
        app_id:        String,
        bypass_policy: bool,
        /// The persona to run the instance in (personas.md §3); absent =
        /// Personal. Each persona's instance is its own jail, uid and data.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        persona:       Option<String>,
    },

    /// Run an installed app in a ONE-SHOT jail wired to the caller's stdio.
    ///
    /// ★★ A SEPARATE REQUEST, NOT A FLAG ON `Launch`, because it is a
    /// different lifecycle rather than a variant of the same one. `Launch` is
    /// an application: one jail per app id, a persistent overlay, a dedicated
    /// per-app uid, first-run setup, single-instance. This is a unit of work:
    /// a per-instance jail and root, a tmpfs upper layer discarded at exit,
    /// no overlay and no first-run, many concurrently from one app. Folding
    /// them together would put a boolean in the middle of the launch path
    /// deciding which half of it to skip.
    ///
    /// `instance` must be distinct across concurrent runs; the caller owns
    /// that. Signatures are REQUIRED on this path whatever the machine is
    /// configured for, because a worker pool launches continuously.
    ///
    /// Stdio handoff is identical to `Launch`: the daemon answers
    /// `ReadyForFds`, the client sends three descriptors over SCM_RIGHTS, and
    /// the jailed entry speaks on them directly.
    ExecInstance {
        app_id:   String,
        instance: Option<String>,
        tmpfs_mb: u32,
        /// Limits forwarded from `portcullis exec --daemon` (§6.5.2e). Absent
        /// on the wire from an older client = none asked.
        #[serde(default)]
        memory_mb: Option<u64>,
        #[serde(default)]
        cpu_pct: Option<u32>,
        #[serde(default)]
        require_memory_limit: bool,
    },

    /// ★ A fetcher of the caller's own: Nuntius (docs/spec/nuntius.md §2),
    /// the system's jailed fetcher, one instance per request. Requires the
    /// caller's INSTALLED manifest to hold `net-fetch`; the caller is
    /// identified by the launch registry (peer uid), never by anything it
    /// sends. The handoff is `ExecInstance`'s: `ReadyForFds`, then three
    /// descriptors (the channel's socket as stdin and stdout, and stderr),
    /// and the fetcher runs until the caller hangs up on this connection.
    /// The fetcher is always the system's installed one (`NUNTIUS_APP`), run
    /// as its own uid under the system worker rule, never as the caller.
    OpenFetcher,

    /// ★ A content decryption module of the caller's own (docs/spec/hospes.md
    /// §3, §8.4): `hospes-cdm` (`HOSPES_CDM_APP`) in a one-shot jail of its
    /// own, as its own uid, never as the caller. Requires the caller's
    /// INSTALLED manifest to hold `protected-media`. portcullisd picks the
    /// library for `key_system` from the system's CDM install, and opens the
    /// store for (caller app, key system, `origin`); the caller names only
    /// the origin, and can reach no other app's stores. The handoff is
    /// `ExecInstance`'s: `ReadyForFds`, then three descriptors: the control
    /// channel's socket, the data channel's socket, and stderr. The CDM runs
    /// until the caller hangs up on this connection.
    OpenCdm {
        key_system: String,
        origin:     String,
    },

    /// "What may I launch?" — the installed-app catalog, for the launcher UI
    /// (Forum's dock). Requires the caller's manifest to hold `app-launch`.
    ///
    /// The daemon answers from its own view of the app tree rather than the
    /// caller reading the tree itself. That is the point: a jailed launcher
    /// never needs the app directory mounted, so it learns exactly what it may
    /// launch and nothing else about the filesystem — and the TCB stays the one
    /// deciding what is listed.
    Catalog,

    /// A memory governor (atrium-memoryd) asks portcullisd to signal a
    /// jaild-created jail's processes — the reclaim cascade. portcullisd checks
    /// the caller is a `memory_govern` service (its services.d manifest's
    /// exec.uid == the peer uid) and forwards to jaild as `Reap`; jaild applies
    /// its own jaild-created-only + `[resource_control]` gates. See
    /// `atrium-memory-pressure.md` §9.5.
    GovernReap {
        jail_name: String,
        signal:    GovSignal,
    },

    /// A memory governor (atrium-memfed) asks portcullisd to set a
    /// jaild-created jail's RCTL `memoryuse` cap (forwarded to jaild as
    /// `SetRctl`). Same `memory_govern` capability gate.
    GovernSetRctl {
        jail_name:    String,
        memoryuse_mb: u64,
    },

    /// The jailed, non-root session launcher (`_ostiarius`) asks portcullisd to
    /// launch a DECLARED session component (vestibulum, forum-wm, …). portcullisd
    /// checks the caller holds `session_launch` (its services.d manifest cap),
    /// looks `component_id` up in the session-component registry — an UNKNOWN id
    /// is refused, so `_ostiarius` can launch only the declared session set, never
    /// an arbitrary jail — fills the per-session owner, and forwards `CreateJail`
    /// to jaild. The procdesc is held by the TCB, not ostiarius. See
    /// docs/spec/ostiarius-privsep.md.
    LaunchSessionComponent {
        component_id: String,
        owner_name:   String,
    },

    /// `_ostiarius` forwards a credential received from vestibulum for portcullisd
    /// to verify against PAM/shadow — which it can read as root; the hashes never
    /// leave portcullisd. Same `session_launch` gate. Returns only yes/no + the
    /// canonical username.
    VerifyCredential {
        user:     String,
        password: String,
    },

    /// `_ostiarius` asks portcullisd to tear down a session component it launched
    /// (logout, or the component exited). portcullisd closes the procdesc it holds
    /// for `jail_name` — killing the persist=0 jail — and RemoveJails it. Same
    /// `session_launch` gate.
    TeardownSessionComponent {
        jail_name: String,
    },

    /// A jailed, non-root broker client (e.g. `_stoad`) asks portcullisd to
    /// exec a shell INSIDE an existing jaild-created jail on a pty — Stoa's
    /// jail-target sessions / `jexec` reimagined (stoa.md §4.5). portcullisd
    /// gates on the `jail_exec` capability (and `jail_exec_root` if
    /// `want_root`), forwards `ExecInJail` to jaild, and relays the two fds
    /// jaild returns — `[procdesc, pty_master]` — back to the caller over
    /// SCM_RIGHTS. The shell runs as the jail's own app-uid (non-root)
    /// unless `want_root`. Reply: `JailExecStarted` then the fds.
    ExecInJail {
        jail_name: String,
        path:      String,
        argv:      Vec<String>,
        #[serde(default)]
        want_root: bool,
        cols:      u16,
        rows:      u16,
    },

    /// ★ The apps that open `mime`, for the file manager's Open and Open
    /// with (docs/spec/opening-files.md §6.1). Only from the session's file
    /// manager (its INSTALLED manifest holds `removable-files`), as for
    /// dispensatord's CONNECT. Answer: `OpenApps`.
    OpenApps { mime: String },

    /// ★ Open a file in an app (opening-files.md §6.2). Only from the
    /// session's file manager, and only with `activation`, a press in its
    /// window that portcullisd spends with frescod (purpose OPEN), so a file
    /// is opened only because the user asked. `app` must declare `mime`
    /// (and `edit` it, for `write`); the app must be one the user's policy
    /// already lets run. Then `ReadyForFds`, one descriptor (the file,
    /// opened by the file manager with `O_NOFOLLOW`), and `Opened`. The app
    /// gets it narrowed, on its document channel. `always` makes `app` the
    /// user's default for `mime` (the Open with sheet's "Always").
    OpenFile {
        app:        String,
        mime:       String,
        /// The file's own name, for the app's title bar: never a path.
        name:       String,
        write:      bool,
        activation: [u8; 16],
        #[serde(default)]
        always:     bool,
    },

    /// ★ Open an attachment (mail.md §8.2). Only from an app whose
    /// installed manifest holds `open-attachments` (Mail), with
    /// `activation`, a press in its window spent here with frescod (purpose
    /// OPEN). Then `ReadyForFds` and one descriptor: a sealed (no write,
    /// grow or shrink), read-only memory file, never a path. portcullisd
    /// decides the type from its bytes and `name` itself, refuses programs,
    /// shortcuts and names that lie, and hands it read-only to the person's
    /// default app for the type, as `OpenFile` does. Answer: `Opened`.
    OpenAttachment {
        /// The attachment's own name: never a path.
        name:       String,
        activation: [u8; 16],
    },

    /// ★ A link to open (opening-links.md §5), first step. Only from
    /// Monstrator (peer uid `_monstrator`). `asker_uid` is the app the
    /// person pressed in, `activation` that press, spent here with frescod
    /// (purpose LINK) for that uid, so Monstrator can't open links on its
    /// own. `scheme` is the link's; `app` names the app for an
    /// `atrium-app:` link. Answer: `LinkGranted`.
    LinkGrant {
        asker_uid:  u32,
        activation: [u8; 16],
        scheme:     String,
        #[serde(default)]
        app:        Option<String>,
    },
    /// ★ Second step: hand `url` to `app` under `grant` (once, within two
    /// minutes). `app` must be one `LinkGranted` listed; `url` must start
    /// with the grant's scheme. `always` makes it the person's default for
    /// the scheme (the sheet's "Always use"). Answer: `LinkDelivered`.
    DeliverLink {
        grant:  [u8; 16],
        app:    String,
        url:    String,
        #[serde(default)]
        always: bool,
    },

    /// ★ A console in an app's jail (docs/spec/homes.md §4): a shell as the
    /// app's uid for this person, on a new pty whose master side goes to the
    /// terminal app. Only from a launcher (its INSTALLED manifest holds
    /// `app-launch`), and only with `activation`, a press on Terminal in its
    /// window that portcullisd spends with frescod (purpose CONSOLE). Never
    /// for a system app (one the installer granted, or one asking for a
    /// system-only capability). Answer: `ConsoleOpened`.
    ///
    /// The terminal (`consoles`) asks for more of an app it shows a console
    /// of, with a key press in its window: `tab_of` a Stoa session of the
    /// person's for `app_id` (another shell in it), or `new_window`.
    Console {
        app_id:     String,
        activation: [u8; 16],
        #[serde(default)]
        tab_of:     Option<String>,
        #[serde(default)]
        new_window: bool,
    },

    /// ★ Settings' Apps page (curia.md §13): every installed app, what it
    /// asks for, and what the person has allowed. Only from an app whose
    /// INSTALLED manifest holds `app-grants`; about the person that app
    /// belongs to. Answer: `Access`.
    Access,
    /// ★ Take away what the person allowed `app_id` (their grant; the
    /// system's can't be). It asks again at its next launch. `app-grants`
    /// only. Answer: `Ok`.
    RemoveAccess { app_id: String },
    /// ★ Allow what `app_id`'s installed manifest asks for, as the person
    /// would at its first launch. `app-grants` only, and only with
    /// `activation`, a press on Allow in the asking app's window that
    /// portcullisd spends with frescod (purpose APPROVE). Never for an app
    /// asking for a restricted capability: only the system grants those.
    /// Answer: `Ok`.
    ApproveAccess { app_id: String, activation: [u8; 16] },
    /// ★ Settings' Links section (opening-links.md §4.4): each link scheme
    /// an installed app takes, those apps, and the person's default.
    /// `app-grants` only, about the person that app belongs to. Answer:
    /// `Links`.
    Links,
    /// ★ Make `app` the person's default for `scheme` links (it must take
    /// them), or with `None` ask each time. `app-grants` only, and only
    /// with `activation`, a press in the asking app's window that
    /// portcullisd spends with frescod (purpose APPROVE). Answer: `Ok`.
    SetLinkDefault {
        scheme:     String,
        #[serde(default)]
        app:        Option<String>,
        activation: [u8; 16],
    },

    /// The person's workspaces (homes.md §5), Scratch made at the first
    /// ask. Asked by a launcher or by the person (not an app). Answer:
    /// `WorkspaceList`. A workspace's terminal is a `Console` whose
    /// `app_id` is `workspace/<name>`.
    Workspaces,
    /// Make a workspace. `network`: "outbound" (the default) or "none".
    WorkspaceNew {
        name:      String,
        toolchain: String,
        #[serde(default)]
        network:   Option<String>,
        #[serde(default)]
        uses:      Vec<String>,
    },
    /// Its settings: what it uses (read-only links, §5.2) and its network.
    /// Takes effect at its next start; a running one is stopped.
    WorkspaceSet {
        name:    String,
        #[serde(default)]
        network: Option<String>,
        #[serde(default)]
        uses:    Option<Vec<String>>,
        /// Share compiled objects with linked workspaces (§5.4, W9).
        #[serde(default)]
        object_cache: Option<bool>,
    },
    /// Stop its jail (its consoles end); its files stay.
    WorkspaceStop { name: String },
    /// Delete it and everything in its `work`. Only with `activation`, a
    /// press on Delete in the asking app's window that portcullisd spends
    /// with frescod (purpose APPROVE). Refused while another workspace
    /// uses it.
    WorkspaceDelete { name: String, activation: [u8; 16] },
    /// A copy of `name` as `new_name` (homes.md §5.4): its settings, its
    /// `work` and its root, build output and all, under its own uid.
    WorkspaceDuplicate { name: String, new_name: String },
    /// Run what the workspace built (homes.md §5.3, W5): `project` is a
    /// folder in its `work` (empty: `work` itself) holding `atrium.toml`
    /// and the entry it names. Installed as the dev app
    /// `dev.<person>.<name>.<id>`, granted what it declares, and started.
    /// From an app, only with `activation`, a press on Run (purpose
    /// LAUNCH); the person's own CLI needs none. Answer: `WorkspaceRan`.
    WorkspaceRun { name: String, #[serde(default)] project: String, #[serde(default)] activation: [u8; 16] },
    /// An IDE (`workspace-exec`, homes.md §5.3, W6) is given workspace
    /// `name`: only with `activation`, a press in the IDE's window that
    /// portcullisd spends with frescod (purpose APPROVE). Its `work` is then
    /// at `/work/<name>` in the IDE's jail, now and at each launch, and the
    /// IDE may `WorkspaceExec` there. Answer: `Ok`.
    WorkspacePick { name: String, activation: [u8; 16] },
    /// Run `argv` in picked workspace `name`'s jail as its uid, in
    /// `work/<cwd>` (`cwd` relative, no `..`). Answer: `WorkspaceExecStarted`,
    /// then over SCM_RIGHTS `[procdesc, stdin, output]`: the process
    /// descriptor (closing it ends the command; it reports POLLHUP when the
    /// command is done), the command's standard input, and its standard
    /// output and error together. Read the answer with
    /// [`read_response_unbuffered`] so the descriptors aren't lost.
    WorkspaceExec { name: String, argv: Vec<String>, #[serde(default)] cwd: String },

    /// The caller's own persona (personas.md §3), read from the launch
    /// registry by its PEER UID, never from anything it sends: an app
    /// that runs once per persona (the Navigator) names its windows by
    /// it. Answer: `Persona`.
    Persona,
}

/// Who allowed what an app has now (curia.md §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessBy {
    /// The person, at a launch or from Settings.
    You,
    /// The system policy (the installer): not the person's to change.
    System,
}

/// One installed app on Settings' Apps page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessEntry {
    pub id:   String,
    pub name: String,
    /// Who allowed it to run as it is; `None` when it would ask first.
    pub by:   Option<AccessBy>,
    /// Everything it asks for, one line each (the launch prompt's words).
    pub asks: Vec<String>,
    /// What it asks for and hasn't been allowed (empty when it runs
    /// unasked). Starts "Manifest has changed …" after an update.
    pub missing: Vec<String>,
    /// The person has a grant for it (Remove takes it away), and when.
    pub yours: Option<String>,
    /// The restricted capabilities it asks for: only the system grants
    /// these, so Settings can't approve it.
    pub restricted: Vec<String>,
}


/// A workspace, for Forum and Scrinium.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name:      String,
    pub toolchain: String,
    /// "personal" until personas exist (personas.md, a proposal).
    pub persona:   String,
    pub network:   String,
    pub uses:      Vec<String>,
    pub running:   bool,
    /// Shares compiled objects with linked workspaces (§5.4, W9).
    #[serde(default)]
    pub object_cache: bool,
}

/// A toolchain New workspace offers; `available` false until its layer
/// can be installed (atrium-devjail.md §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toolchain {
    pub id:        String,
    pub name:      String,
    pub available: bool,
}

/// An app that opens a type, for the Open with sheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkChoice {
    pub id:      String,
    pub name:    String,
    /// The person's default for the scheme (or the system's, unchosen).
    pub default: bool,
}

/// A link scheme in Settings' Links section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkScheme {
    pub scheme: String,
    /// The apps that take it, by name; `default` marks the one that opens
    /// it now (none: Monstrator asks).
    pub apps:   Vec<LinkChoice>,
    /// The default is the person's choice, not the system's.
    pub yours:  bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenChoice {
    pub id:      String,
    pub name:    String,
    /// It edits the type (`mode = "edit"`).
    pub edit:    bool,
    /// The user's default for the type (or the system's, unchosen).
    pub default: bool,
}

/// One installed app, as the daemon is willing to describe it to a launcher.
///
/// Deliberately just what a launcher must draw and act on — id, name, blurb,
/// icon. No entry path, no bundle hash, no capability set: the launcher only
/// ever names an app back to the daemon, so nothing here needs to describe how
/// it would run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    pub icon:        Option<String>,
    /// The uid this app runs as for the asking person (the launch registry's
    /// binding), `None` until it first ran. Surfaces carry their owner's uid,
    /// so this is how a launcher tells which apps have windows up (forum.md
    /// §6.1) without reading the registry, which a jailed launcher cannot.
    #[serde(default)]
    pub uid:         Option<u32>,
    /// The app's own icon when `icon` is a path in its bundle: the SVG's text
    /// (at most `portcullis_toml::icon::MAX_ICON_BYTES`), read by the daemon
    /// because a jailed launcher cannot see the bundle. The launcher parses it
    /// under a strict profile; `None` → the system icon set or a letter tile.
    #[serde(default)]
    pub icon_svg:    Option<String>,
    /// The app may have a console (homes.md §4, decision H5): a launcher
    /// shows Terminal for it.
    #[serde(default)]
    pub console:     bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Handshake reply; daemon advertises its proto version.
    Hello { version: u32 },
    Pong,
    /// Authorize → all requested caps already granted.
    Authorized,
    /// Authorize → needs user approval. `delta` is human-readable
    /// reason lines (suitable for a prompt UI or CLI message).
    NeedsApproval { delta: Vec<String> },
    /// Grant or Revoke → success.
    Ok,
    /// Generic error reply with a message.
    Error { message: String },
    /// Wire-version mismatch; client should disconnect.
    ProtoMismatch { server_version: u32 },
    /// Daemon doesn't recognize this op (forward-compat).
    UnknownOp,
    /// Launch → app exited with this status.
    /// `code = None` if the jail terminated by signal (Unix
    /// convention: signal-exit doesn't have a numeric exit code).
    LaunchExit { code: Option<i32> },
    /// Launch → setup or teardown failed before/after the app ran.
    LaunchFailed { stage: String, message: String },
    /// Launch handshake: daemon has read+parsed the Launch request,
    /// passed the policy gate, and is now blocked in `recv_fds`
    /// waiting for the client's stdio. The CLI replies by sending
    /// the SCM_RIGHTS sendmsg with the three fds. This round-trip
    /// drains the daemon's BufReader so the cmsg's 1-byte payload
    /// can't be silently swallowed by a buffered read.
    ReadyForFds,
    /// Catalog → the apps this caller may launch.
    CatalogList { apps: Vec<CatalogEntry> },

    /// Launch → the app is `instances = "single"` and one is already running.
    ///
    /// Refusing is correct; reporting it as a failure is not. Without this, a
    /// duplicate launch surfaced as a raw jail(8) "already exists" line plus a
    /// generic non-zero exit, which a launcher cannot tell apart from a broken
    /// app — so clicking a dock icon twice showed an error instead of raising
    /// the window that was already there.
    ///
    /// `uid` is the running instance's per-app uid. That is what makes this
    /// actionable: surfaces carry `owner_uid`, so a launcher can find the live
    /// window and focus it without needing the launch registry (which a jailed
    /// launcher cannot read).
    AlreadyRunning { app_id: String, jid: i32, uid: u32 },

    /// OpenApps → the apps that open the type, the default first.
    OpenApps { apps: Vec<OpenChoice> },
    /// LinkGrant → a grant for one link, for `person`, and the apps that
    /// take the scheme there (the asker left out), the default first.
    LinkGranted { grant: [u8; 16], person: String, apps: Vec<LinkChoice> },
    /// DeliverLink → handed to `app` (its name).
    LinkDelivered { app: String },
    /// Links → each scheme an app takes, by scheme.
    Links { schemes: Vec<LinkScheme> },
    /// OpenFile → handed to `app` (started for it, or already running).
    Opened { app: String, name: String },
    /// Console → the shell runs; its terminal window is `title`.
    ConsoleOpened { title: String },
    /// A Run started: the dev app's id and what it was granted.
    WorkspaceRan { app_id: String, capabilities: Vec<String> },
    /// A `WorkspaceExec` started (pid in the workspace's jail); three
    /// descriptors follow.
    WorkspaceExecStarted { pid: i32 },
    /// Access → each installed app, by name.
    Access { apps: Vec<AccessEntry> },

    /// Answer to `Workspaces` (and to `WorkspaceNew`/`WorkspaceSet`: the
    /// list after the change).
    WorkspaceList { workspaces: Vec<WorkspaceInfo>, toolchains: Vec<Toolchain> },

    /// Launch → policy gate refused. `delta` is the human-readable
    /// list of capabilities the user hasn't granted yet (suitable
    /// for showing in a prompt UI). Distinct from LaunchFailed —
    /// this is "needs interactive approval", not a permanent error.
    /// The CLI may prompt the user and re-issue Launch with
    /// bypass_policy=true (Allow Once) or call Grant first then
    /// re-issue (Allow Always).
    LaunchNeedsApproval { delta: Vec<String> },

    /// LaunchSessionComponent → the session jail was created. The procdesc is
    /// held by the TCB; ostiarius gets the identifiers for status/teardown.
    SessionComponentLaunched { pid: i32, jid: i32, jail_name: String },
    /// VerifyCredential → the credential is valid; the canonical username.
    CredentialVerified { user: String },

    /// ExecInJail → the shell is running inside the jail on a pty. Two fds
    /// follow over SCM_RIGHTS, in order: `[procdesc, pty_master]`. `uid` is
    /// the uid the shell actually runs as inside the jail (the jail's
    /// app-uid, or 0 if `want_root` was granted) — for the caller's logs.
    JailExecStarted { pid: i32, uid: u32 },

    /// Persona → the caller's: `None` for Personal, else the named
    /// persona and its colour (one of portcullis-policy's
    /// `PERSONA_COLOURS`).
    Persona { name: Option<String>, colour: Option<String> },
}

/// Send one request and read one response over a connected stream.
/// Convenience for synchronous client code.
pub fn round_trip(stream: &mut UnixStream, req: &Request) -> io::Result<Response> {
    write_request(stream, req)?;
    read_response(stream)
}

pub fn write_request<W: Write>(w: &mut W, req: &Request) -> io::Result<()> {
    let mut line = serde_json::to_vec(req).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> io::Result<()> {
    let mut line = serde_json::to_vec(resp).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Request> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed"));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                 format!("bad request frame: {e}")))
}

/// [`read_response`] without read-ahead: one byte at a time up to the
/// newline, so data (and descriptors) sent after the answer stay on the
/// socket for [`recv_fds`].
pub fn read_response_unbuffered<R: Read>(r: &mut R) -> io::Result<Response> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed"));
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        if line.len() > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "response frame too long"));
        }
    }
    serde_json::from_slice(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad response frame: {e}")))
}

pub fn read_response<R: Read>(r: &mut R) -> io::Result<Response> {
    let mut br = io::BufReader::new(r);
    let mut line = String::new();
    let n = br.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed"));
    }
    serde_json::from_str(line.trim_end())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                 format!("bad response frame: {e}")))
}

use std::io::Read;

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_toml::NetworkCap;

    #[test]
    fn exec_instance_limits_are_optional_on_the_wire() {
        // An older CLI sends no limits; it must still parse, as "none asked".
        let r: Request = serde_json::from_str(
            r#"{"op":"exec_instance","app_id":"a","instance":null,"tmpfs_mb":64}"#).unwrap();
        match r {
            Request::ExecInstance { memory_mb, cpu_pct, require_memory_limit, .. } =>
                assert_eq!((memory_mb, cpu_pct, require_memory_limit), (None, None, false)),
            other => panic!("{other:?}"),
        }
        let s = serde_json::to_string(&Request::ExecInstance { app_id: "a".into(), instance: None, tmpfs_mb: 64,
            memory_mb: Some(512), cpu_pct: Some(100), require_memory_limit: true }).unwrap();
        assert!(s.contains("\"memory_mb\":512") && s.contains("\"cpu_pct\":100") && s.contains("\"require_memory_limit\":true"), "{s}");
    }

    #[test]
    fn roundtrip_authorize_request() {
        let mut caps = Capabilities::default();
        caps.clipboard = Some(true);
        caps.network   = Some(portcullis_toml::NetworkSpec::Mode(NetworkCap::Loopback));
        let req = Request::Authorize {
            app_id:        "org.atrium.edit".into(),
            manifest_hash: "sha256:abc".into(),
            requested:     caps,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"op\":\"authorize\""));
        let back: Request = serde_json::from_str(&s).unwrap();
        match back {
            Request::Authorize { app_id, .. } => assert_eq!(app_id, "org.atrium.edit"),
            _ => panic!("wrong variant"),
        }
    }

    /// Lock the governor-broker wire format (portcullisd + the daemons depend on
    /// it): the `op` tags and the bounded GovSignal round-trip.
    #[test]
    fn roundtrip_govern_requests() {
        let r = Request::GovernReap {
            jail_name: "app-org-atrium-web".into(),
            signal: GovSignal::Exit,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"govern_reap\""));
        assert!(s.contains("\"signal\":\"exit\""));
        match serde_json::from_str::<Request>(&s).unwrap() {
            Request::GovernReap { jail_name, signal } => {
                assert_eq!(jail_name, "app-org-atrium-web");
                assert_eq!(signal, GovSignal::Exit);
            }
            _ => panic!("wrong variant"),
        }

        let r = Request::GovernSetRctl {
            jail_name: "app-org-atrium-web".into(),
            memoryuse_mb: 2048,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"govern_set_rctl\""));
        match serde_json::from_str::<Request>(&s).unwrap() {
            Request::GovernSetRctl { memoryuse_mb, .. } => assert_eq!(memoryuse_mb, 2048),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_needs_approval_response() {
        let r = Response::NeedsApproval {
            delta: vec!["Use the fresco graphics service".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back {
            Response::NeedsApproval { delta } => assert_eq!(delta.len(), 1),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn newline_framing_via_pipe() {
        use std::io::Cursor;
        let req1 = Request::Ping;
        let req2 = Request::Reload;
        let mut buf = Vec::new();
        write_request(&mut buf, &req1).unwrap();
        write_request(&mut buf, &req2).unwrap();
        let mut br = io::BufReader::new(Cursor::new(buf));
        let r1 = read_request(&mut br).unwrap();
        let r2 = read_request(&mut br).unwrap();
        matches!(r1, Request::Ping);
        matches!(r2, Request::Reload);
    }
}
