# Roadmap — Atrium

Phases are coherent deliverables. Don't start the next until the previous demos. Honest about scope: this is years of work.

Each phase lists:
- **Deliverable** — the visible outcome.
- **Prerequisites** — what must exist before starting.
- **Scope** — concrete sub-tasks.
- **Risks** — what's hard or unknown.
- **Estimate** — focused-engineer-months. Concurrent helpers shorten.

For component naming (Portcullis, Castellum, Vestibulum, ...) see [NAMING.md](NAMING.md).

## Phase 0 (DONE 2026-04-28) — Multi-client desktop POC

End-to-end Fresco protocol with macOS host (Metal backend) + FreeBSD guest (QEMU).

- Server: retained-mode scenegraph, per-window FBOs, WM, multi-client per-slot ring isolation, DMA upload, client-disconnect cleanup, drag-to-resize.
- Kmod: native newbus/kqueue/cdev, no linuxkpi.
- Apps: atrium-edit + atrium-term, side-by-side, isolated input, kernel-pty `TIOCSWINSZ` verified.

Documented in [ARCHITECTURE.md](ARCHITECTURE.md), runtime details in [RUNBOOK.md](../RUNBOOK.md).

## D0 — Native FreeBSD kernel GPU ABI + virtio-gpu driver

**Deliverable:** the Fresco display server runs on bare-metal FreeBSD against a from-scratch native virtio-gpu driver. No linuxkpi. No DRM/KMS. No drm-kmod. `pkg install atrium` on a FreeBSD VM with virtio-gpu → Atrium apps display on the VM's actual screen.

**Prerequisites:**
- Fresco wire-format spec frozen (see [spec/wire-format.md](spec/wire-format.md)).
- Atrium GPU ABI designed (see [spec/gpu-abi.md](spec/gpu-abi.md)).

**Scope:**
1. Spec the cdev ABI: memory alloc, command submit, fence sync, modeset, vblank, interrupt → kqueue.
2. New kmod for `/dev/atrium-gpu0` (parallel to today's `/dev/fresco0` transport cdev).
3. `virtio-gpu` driver against the spec. Document at the cmd-buffer level (it's a small spec).
4. Fresco server `gpu_backend::virtio` impl. Same `GpuBackend` trait as Metal.
5. Modesetting: enumerate displays, set mode, page flip. Driven by server, not legacy `vt(4)`.
6. Hardware cursor support (virtio-gpu has it).
7. Bare-metal-FreeBSD bring-up checklist + RUNBOOK section.

**Risks:**
- Modesetting from userspace is unusual for FreeBSD; need to think through privilege boundary. Likely the server runs as a dedicated user with cdev access, not as root.
- virtio-gpu has a 2D mode and a 3D mode (VirGL). Start with 2D + textured-quad blits; add 3D later.
- Documentation of FreeBSD's PCI BAR / interrupt / DMA APIs (without linuxkpi) is sparse; expect to read source.

**Estimate:** 3–4 months focused.

## D1 — Native FreeBSD bare-metal server

**Deliverable:** the Fresco display server runs natively on FreeBSD (no QEMU, no macOS host) on a real machine, displaying to a real monitor. atrium-edit + atrium-term usable.

**Prerequisites:** D0.

**Scope:**
1. Replace winit-managed window on macOS with direct cdev modesetting.
2. Native FreeBSD input: usbhid/hkbd → HID-tagged events. No evdev.
3. Audio cdev (a separate sub-project; can stub).
4. Power management hooks (suspend/resume).
5. Optionally: Vulkan-on-linuxkpi backend as a fallback for hardware D0 doesn't cover yet — explicitly the **deprecation path**.

**Risks:**
- Going from "winit-on-macOS" to "raw modesetting on FreeBSD" is a step in the dark for a lot of plumbing (event-loop, DPI scaling, multi-monitor coordinates).
- HID over usbhid: have the HID parsing helpers in libusb / libusbhid; FreeBSD-native.

**Estimate:** 1.5–2 months focused.

## D1.5 — Tessera (content-addressed filesystem)

**Deliverable:** install atrium-edit twice with different libc versions; on-disk delta is just the unique files. `pkg install atrium-edit-1.2.4` after `atrium-edit-1.2.3` transfers only delta tesserae.

**Prerequisites:** None (independent of D0/D1).

**Scope:**
1. Survey Karythra's CAS-FS implementation as the reference.
2. Decide: port to FreeBSD (in-kernel), or wrap with FUSE (userspace), or stack nullfs+unionfs over a tessera-store directory (degenerate but functional).
3. Tessera store layout: `/var/lib/tessera/cas/<sha256>/data` content blobs, jail trees as directories of symlinks (or hardlinks where idempotent).
4. CLI tools: `tessera import <dir>` (deduplicates), `tessera stat <hash>`, `tessera gc`.
5. Performance baseline: hot-path read latency, lookup overhead vs. UFS direct.

**Risks:**
- FUSE has overhead. In-kernel is better but bigger commitment. Ship FUSE first, in-kernel as a follow-on.
- GC of unreferenced tesserae is its own design problem.

**Estimate:** 1.5–2 months focused.

## D1.6 — Atrium-RPC (unified IPC substrate)

**Deliverable:** every Atrium service (Fresco today; clipboard, notify, broker, audio control plane tomorrow) speaks the same wire envelope. CAS-keyed payloads share a hash space with Tessera so the same image referenced via clipboard, notification, and editor preview is one allocation. fd-passed shm handles high-bandwidth payloads. Capability is filesystem-enforced by Portcullis (D2.5) — apps see only the service sockets their manifest declares.

Spec: [`spec/atrium-rpc.md`](spec/atrium-rpc.md).

**Prerequisites:** D1 software thesis (Fresco running natively on FreeBSD). D1.5 Tessera (for the optional zero-copy hash-via-CAS path).

**Scope:**
1. Spec freeze. `docs/spec/atrium-rpc.md` reviewed and committed; `atrium-rpc-core/src/classes.rs` enumerates `opcode_class` constants.
2. `atrium-rpc-core` crate scaffold. Envelope codec, `Connection` type, CAS upload/fetch state machines, async event channel, fd-pass helper.
3. Extract reusable layers from fresco-socket-rs into atrium-rpc-core. Keep fresco-socket-rs as a thin layer over atrium-rpc-core that adds the display opcode dictionary (class 1).
4. Verify all existing demos (rect-bouncer, slot-demo, edit-socket, textured, window-demo, keyboard) work unchanged on top of the refactored stack.
5. Document patterns for downstream services (`docs/spec/atrium-rpc-services.md`).

**Risks:**
- Refactor of the Fresco wire-format code. Existing demos + atrium-edit-socket must keep working.
- The "CAS hashes are advisory pointers" semantic must be enforced rigorously (verify-on-use, sender-must-serve).
- Opcode_class allocation needs a single source of truth — both this doc and `atrium-rpc-core/src/classes.rs`.

**Estimate:** 2–3 weeks focused.

## D2 — Vestibulum (display manager + auth)

**Deliverable:** boot FreeBSD → see a Fresco-rendered login screen → enter credentials via PAM → start a user session.

**Prerequisites:** D1.

**Scope:**
1. `vestibulum`: an Atrium app started by `init` (or `rc`).
2. PAM integration. Standard FreeBSD PAM stack.
3. Session handoff: post-auth, vestibulum `setuid`s to the user, `execve`s a per-user supervisor.
4. Privilege boundary design: vestibulum owns the GPU pre-auth; the session owns it post-auth. Reset compositor state at handoff.

**Risks:**
- The privilege handoff is tricky. The cdev was opened by vestibulum; we need to either re-open as the user or hand off the fd carefully.
- Login-screen UI itself is an Atrium app — needs the foundation libs.

**Estimate:** 1 month focused.

## D2.5 — Portcullis (jail launcher + capability manifest)

**Deliverable:** `portcullis launch <app>` reads the app's `atrium.toml` manifest, builds a jail with the declared capabilities, execs the app inside. Forum (the dock, D3) calls Portcullis to launch apps.

**Prerequisites:** D1.5 (Tessera for jail trees) + D1.6 (atrium-rpc — defines the per-service-socket convention that capability mounts target).

**Scope:**
1. `atrium.toml` schema: graphics/filesystem/network/devices/audio/IPC capabilities.
2. Jail builder: takes manifest + app's tree path; constructs a `jail.conf` invocation.
3. devfs.rules wiring: `/dev/fresco0` (and other selected cdevs) into the jail.
4. **Per-capability nullfs of `/atrium/sockets/<service>.sock`** — the kernel-enforced IPC capability boundary defined by atrium-rpc.
5. Optional `tessera-cas-read` capability: nullfs of `/var/lib/tessera/cas` for trusted system services (clipboard, notifier, thumbnailer) so they can answer hash lookups without going through the wire.
6. Capability prompt UI: "first launch, this app wants network — allow?". Subsequent launches: silent unless manifest changed.
7. Filesystem mounts: nullfs from CAS-FS into the jail; per-app writable overlay.
8. Resource limits (rctl).

**Risks:**
- Capability UX is hostile if there are too many prompts. Group and default-grant the obvious ones.
- A misconfigured manifest could trap the app in an unworkable state. Need a "permissive mode" for development.

**Estimate:** 1 month focused.

## D3 — Forum (shell) + Praeco (notifications)

**Deliverable:** after login: wallpaper + clock/status bar + app launcher dock. Click an icon, Forum asks Portcullis to launch the app in its jail, the app's window appears. Praeco renders transient toasts.

**Prerequisites:** D2 + D2.5.

**Scope:**
1. `forum` (single binary covering wallpaper + statusbar + dock):
   - wallpaper: paints the screen background with image / solid.
   - statusbar: clock, battery, network, volume, notification stack.
   - dock: reads `~/.local/share/atrium/apps/*.toml`, shows icons, asks Portcullis to launch.
2. `praecod` — notification daemon; toast rendering + history.
3. Window-snap, minimize-to-dock, maximize. Already have move + resize + close from earlier.

**Risks:** Mostly polish. No big architectural unknowns.

**Estimate:** 1.5 months focused.

## D4 — Foundation apps

**Deliverable:** the apps a user opens on day 1.

- `atrium-files` — file manager. Built atop Scrinium; jail-scoped path access.
- `curia` — settings (display, keyboard, audio, network, capability inspector).
- `atrium-image` — image viewer.
- `atrium-pdf` — PDF viewer (wrap mupdf).
- `lyra` + `tabula` baseline ship here too: audio server and clipboard service so the foundation apps actually work end-to-end.

**Prerequisites:** D3.

**Estimate:** 1.5–2 months total. Each app is small; cumulative.

## D5 — Slint backend for Fresco

**Deliverable:** Slint (`https://slint.dev/`) is a retained-mode UI framework with its own scene graph. Write a Fresco backend so any Slint app runs on Atrium unchanged. This is the moment the ecosystem inflects.

**Prerequisites:** D1.

**Scope:**
1. Implement Slint's `Renderer` trait against fresco-rs.
2. Map Slint's animation timeline → Fresco transform updates.
3. Map Slint's brushes/gradients/clip → Fresco materials.
4. Demo: existing Slint examples (clock, todo, gallery) run on Fresco-on-FreeBSD with no source change.

**Risks:**
- Custom shaders in Slint may need protocol extensions.
- Text rendering: Slint uses its own text shaping; map to fresco-text.

**Estimate:** 1 month focused.

## D6 — Browser (Servo + Fresco backend)

**Deliverable:** a working web browser on Atrium. Single biggest credibility test.

**Prerequisites:** D5 (helps but not strictly required).

**Scope:**
1. WebRender (Servo's GPU compositor) abstracts over a "renderer" trait. Write a Fresco backend.
2. WebRender is already retained-mode and uses a render-graph approach — good fit.
3. Servo on FreeBSD: depends on rustc + cargo (have them), plus a few system libs (mostly available).
4. Iterate on real sites: text-heavy first (Wikipedia), then JS-heavy, then video.

**Risks:**
- Servo is a research browser; site-compat is incomplete. Worth being explicit that it won't render every site at v1.
- WebRender's GPU abstraction may not map 1:1 to scenegraph. Some shader work likely.
- Browsers are huge. Expect 6+ months of focused work; partnerships welcome.

**Estimate:** 6 months focused (long pole).

## D7 — Standardization

**Deliverable:** the protocol is a versioned, documented spec; vendors and standards bodies can evaluate.

**Prerequisites:** D6 ideally — by then we have a credible "real apps work" story.

**Scope:**
1. Wire-format spec — versioned, opcode-by-opcode, normative.
2. Reference implementation = our server. Conformance test suite.
3. Spec for the kernel/userspace GPU ABI (companion to wire-format).
4. Submit to a standards body. Most natural home is FreeDesktop.org for the windowing/protocol piece, possibly Khronos for the GPU-IO piece. Probably both.
5. Find allies: Slint team (likely receptive), Servo team, embedded GPU vendor (Imagination?, ARM?), cloud-desktop vendor (Frame, Cameyo, Citrix).
6. Paper / writeup for academic visibility.

**Risks:**
- Standards work is slow and political. Treat it as a parallel effort, not a dependency.
- Vendor adoption is the harder lift than spec ratification.

**Estimate:** indefinite — measured in years, not months.

## Dependencies

```
D0 ──► D1 ──► D2 ──► D3 ──► D4
        │      ▲
        ▼      │
       D1.5 ──► D2.5
        │
        ▼
       (D5 depends on D1)
       (D6 depends on D5 helpful, D1 required)
       (D7 ideally after D6)
```

D1.5 (Tessera) and D2.5 (Portcullis) are independent of D0 and could even precede it on a Linux dev machine; we keep them in the FreeBSD-native track because the integrated platform story requires all three.

## What's not on the roadmap (yet)

Conscious omissions, to be added when prioritized:

- **Lyra (audio) full mixer.** Baseline ships in D4; full per-stream mix + routing is its own milestone. Reuses the slot/ring transport idea.
- **Networking notification UI / VPN / etc.** — system-services UI. Each is a small Atrium app talking to a privileged service over Castellum.
- **Multi-monitor.** The compositor today knows one screen window 0. Need to generalize.
- **Accessibility.** Semantic tree alongside the visual tree. Big design space.
- **IME / complex text input.** Composition + commit events. Protocol extension.
- **Tabula (clipboard) + drag-and-drop.** Clipboard entries are CAS blobs with format declarations; DnD piggybacks the same shape.
- **More GPU drivers.** Beyond virtio-gpu (D0): Raspberry Pi VideoCore VI/VII, Mali via Panfrost docs, AMD RDNA via partnerships, then Intel, NVIDIA, Apple Silicon (Asahi-style RE).
- **Linux app compat.** Linuxulator + a Fresco-X11/Wayland bridge. Optional, but a viable bridge for ecosystem pull-through.
- **Opifex (package manager) full implementation.** Distribution is jail trees; tooling for fetch / verify / install / update / rollback. Bootstrapped by `pkg` early; full Opifex is its own milestone.

These slot in opportunistically, not blocking the main spine.
