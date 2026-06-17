# Language policy

> **Kernel = C. Userspace = Rust by default. Public APIs = C ABI.**
>
> Carve-out: C is acceptable in userspace where (a) inherited from upstream, (b) the code lives in the smallest-TCB tier and a maintainer makes a written case for it, or (c) it's a thin C-ABI shim layer over a Rust implementation.

This document records a one-time architectural decision so contributors know the rule and the reasoning behind it. Re-litigating the question costs more than it saves; that's the only reason this document exists.

Last revised: 2026-05-07. Revisions are themselves rare and require a written case (see §Practical consequences).

## The rule

| Layer | Language | Why |
|---|---|---|
| Kernel modules (`atrium-kmod`, future GPU/display/Tessera drivers) | **C** | Required — FreeBSD kernel doesn't accept Rust |
| Public ABIs (`libfresco`, `libatrium-gpu`, `libcastellum`, ...) | **C headers** | Universal consumption — vendors, contributors, other languages |
| Rust binding crates (`fresco-rs`, `atrium-gpu-rs`, ...) | Rust | Ergonomic safe wrappers over the C ABIs |
| Display server (`fresco-server`) | **Rust** | Multi-client, multi-threaded, performance-critical, security-critical |
| Privileged userspace daemons (`portcullisd`, `castellumd`, `vestibulumd`, `lyrad`, `tabulad`, `praecod`, `opifexd`, `curiad`, `scriniumd`) | **Rust** | Same reasons as the server, doubly so for jail launcher / IPC bus |
| Shell + foundation apps (`forum`, `atrium-edit`, `atrium-term`, `atrium-files`, `atrium-image`, `atrium-pdf`, `atrium-clock`) | **Rust** | Already built; ecosystem alignment with the permissive Rust toolkit/engine world (winit/wgpu, egui, iced, Bevy) / Servo |
| Wire-format and ABI specifications | **Language-agnostic** | A conformant C, Go, Zig, or Swift impl is welcome |

## Why Rust for userspace

1. **Memory safety in a 1000-jailed-app world.** Atrium's thesis is "every app is its own jail." That model requires the privileged daemons to be unimpeachable — UAF, double-free, and buffer overflow in `portcullisd` or `fresco-server` would defeat the whole architecture. The bug classes Rust eliminates are exactly the classes that have plagued X servers and Wayland compositors for decades.
2. **Concurrency without data races.** The display server has per-slot rings, multi-client fan-in, per-window FBOs, and shared content-addressed stores. `Send`/`Sync` enforcement is a real correctness tool, not a luxury — it's how we get away with doing the right thing rather than the cheap thing.
3. **Ecosystem alignment.** The permissive Rust toolkit/engine ecosystem we import via the backend-multiplier (winit/wgpu, egui, iced, Bevy; D5 — see `spec/toolkit-backends.md`) and Servo + WebRender (D6) are Rust. Modern text and graphics tooling — `lyon`, `rustybuzz`, `swash`, `tiny-skia`, `cosmic-text` — is Rust-first. Asahi-style GPU reverse engineering is Rust-first. Picking C means re-implementing that stack ourselves.
4. **Already shipped.** `fresco-server`, the four foundation apps, `fresco-rs`, `fresco-text`, and `atrium-gpu-rs` are Rust. Switching is enormous cost for arguable gain.
5. **Industry direction.** The Linux kernel accepts Rust. Microsoft is shipping Rust in Windows. Google ships Rust in Android. Betting on Rust as a systems language in 2026 is no longer speculative.

## The smallest-TCB carve-out

Added 2026-05-07 after the privsep-architecture work for D2.5 (see
`docs/spec/portcullis.md` §0.5 and `scratch/jail-smoke/`).

Some Atrium components are the *smallest, most-trusted* tier of
userspace — code that, if compromised, gives an attacker
arbitrary-jail-creation, arbitrary-credential-verification, or
similar root-of-trust privileges. The canonical examples are
`jaild` (sole `jail_set(2)` caller) and the deferred `atrium-authd`
(sole `crypt(3)` + master.passwd reader).

For these specifically, C is **defensible but not preferred**. The
honest argument for each:

**Pro-C for smallest-TCB code:**
- Auditability: a FreeBSD-shaped reviewer reads C natively.
- TCB minimization: the most-trusted code shouldn't depend on a
  large compiler toolchain (rustc) any more than necessary.
- Tradition: OpenBSD privsep monitors are all small audited C.
  Existence proof of the approach.

**Pro-Rust for smallest-TCB code (and why we still pick Rust):**
- Untrusted-input parsing dominates the LoC of these daemons.
  jaild's policy file is TOML; atrium-authd takes credential bytes
  from vestibulum. Hand-rolling parsers in C without `serde`-style
  tooling is exactly the CVE source we're trying to avoid.
- We depend on rustc for the rest of the platform anyway. Adding
  a C component doesn't remove the rustc dependency, just adds
  a different toolchain.
- 500 LoC of Rust audits about the same as 500 LoC of C, with the
  bonus that bounds checks aren't a manual-review responsibility.

**Resolution:** smallest-TCB code **is** Rust, but with extra
discipline:

1. `#![forbid(unsafe_code)]` at the crate root, with a clearly-
   named `unsafe` syscall-wrapper module (e.g. `mod ffi;`) being
   the only exception.
2. No async runtime (no `tokio`, no `async-std`). Use `std::thread`
   + blocking I/O, or `std::os::unix::io` with `kqueue` directly.
3. Minimal external dependencies: `libc`, `serde`, `toml`,
   `thiserror` is a typical full set. Each new dep needs a written
   justification.
4. No clever traits, no procedural macros beyond the dep set above,
   no GATs, no async fn. Aim for "C with safety" reading style;
   a FreeBSD developer should find the control flow obvious.
5. `CONTRIBUTING.md` in the crate documents these rules, with the
   reasoning, so future contributors don't accidentally pull the
   crate in a different direction.

**Carve-out invocation procedure:** if a maintainer wants a
new component to be C, they write a section in that component's
top-level README: "Why this is C", linking to this policy file.
The case must address (a) why the smallest-TCB argument
specifically applies, and (b) how the parser-attack-surface concern
is handled (typically: "no parsing of structured input"). PRs to
new C-only Atrium-authored userspace components are reviewed
against this written justification.

To date, no carve-out has been invoked. jaild and atrium-authd are
both Rust per §The smallest-TCB carve-out resolution above.

## Vendored / forked dependencies stay in their native language

The rule above governs **Atrium-authored** code. Code we vendor or fork from
upstream (e.g. `atrium-mesa` forking Mesa, future kernel-driver imports)
keeps its upstream language — we don't rewrite working C/C++ for ideological
reasons. Specifically:

- **Atrium-authored userspace** → Rust (per the rule).
- **Forks we maintain** (atrium-mesa is the canonical example) → inherit the
  upstream language. New code *we* add into those forks follows the Rust
  default where natural.
- **Where upstream is itself moving toward Rust** (Mesa's `nak` compiler is
  Rust, more conversions discussed), we adopt and contribute to that
  movement rather than running our own parallel rewrite.

This is the same pragmatism as "we don't rewrite FreeBSD base." The thing
that makes Atrium *Atrium* is the layer above; the layers below stay
themselves.

## Why C at the boundaries

This is the answer to "FreeBSD is a C culture; won't Rust create friction?"

We're not modifying FreeBSD base. Atrium is a parallel stack on top. The ports tree already carries thousands of Rust applications. Friction is mitigated by keeping the *contract* in C:

- Every Atrium service that is a useful integration point ships a **C-callable library** with a stable C header — `libfresco.h`, `libatrium-gpu.h`, `libcastellum.h`. A C, Zig, Go, or Swift program can build against Atrium without ever touching Rust.
- Every wire-format spec describes byte layouts, not Rust types. `repr(C)` types in our Rust code reproduce the spec; they aren't the source of truth.
- Kernel modules — the layer that touches FreeBSD's actual code — are C. There is no Rust-in-the-kernel proposal in this document.

So a C-preferring contributor can:
- Implement a Fresco client in C (already supported via `libfresco`).
- Write a system service that speaks Castellum in C (`libcastellum` will exist).
- Author a kernel driver targeting the Atrium GPU ABI in C (no other choice).
- Build their own desktop on top of Fresco in any language.

The only friction is "you can't write a privileged Atrium daemon for our reference desktop in C, because we already wrote it in Rust." That's a packaging choice, not an exclusionary one.

## Shader source language

Shaders compile to SPIR-V (Vulkan's native input). The source
language is **Slang** as of 2026-05-07. Slang is Khronos-stewarded,
Apache-2.0 with LLVM exception (matches Atrium's permissive-only
licensing policy), has no GL/DX heritage, and emits to SPIR-V plus
DXIL / Metal / CUDA / GLSL / HLSL — opening the door to Atrium
running on non-Vulkan backends (Metal native, Direct3D guest)
without re-authoring shaders.

Pre-2026-05-07 history: shaders were authored in GLSL via
glslangValidator. The GL ancestry was a cosmetic concern (modern
Vulkan-flavoured GLSL is essentially a separate language) but Slang's
multi-backend emit is a genuine architectural upside. Migration cost
was small (~7 shaders, all under 100 lines each), and folded a
pre-existing glyph-rendering bug fix into the same change.

`rust-gpu` remains a tracked future option: write shaders in an
actual Rust subset, fully aligning with the userspace-Rust policy.
Deferred pending ecosystem maturity (revisit post-D3).

### Slang invocation rules (important)

`bundles/*/build.sh` invokes slangc as:

```
slangc <src>.slang -target spirv -entry <name> -stage <stage> -o <out>.spv
```

**Do not pass `-profile glsl_460`.** That flag forces Slang into
GL-mimic mode which emits the legacy `BufferBlock` decoration +
`Uniform` storage class for storage buffers. fresco-vulkan's reflect
module treats that pair as a UBO (because it is, by name) and the
descriptor pool runs out. Without `-profile glsl_460`, Slang emits
the modern Vulkan-1.1+ `Block` decoration + `StorageBuffer` storage
class, which is what glslangValidator-with-vulkan1.3 was producing
and what the reflector expects.

## What this is NOT

- **Atrium is not a "Rust OS."** It's not Redox. The kernel is FreeBSD; large parts of base are C; the ports tree is mostly C. We are not on a mission to rewrite anything that already works.
- **Atrium does not require C contributors to learn Rust.** A C developer can ship a kmod, a Fresco client, or a Castellum service against the public ABIs. The Latin-named privileged daemons are written in Rust because *we* wrote them in Rust; that's not a barrier to entry for someone wanting to add a different daemon to the platform.
- **Atrium does not freeze on Rust forever.** If a future Rust ABI stability story or a different language eats Rust's lunch, this doc gets revised. The current decision is right for the current decade.

## Practical consequences

- New service from scratch → Rust crate, vendored deps via `cargo`, published as a port that wraps the cargo build.
- New kernel driver → C source under `atrium-kmod/`, `bsd.kmod.mk` Makefile.
- New public API → C header under each repo's `include/`, with a Rust binding crate alongside (`<service>-rs`).
- New smallest-TCB component (rare; jaild and the deferred atrium-authd are the only known cases) → Rust with the discipline rules from §The smallest-TCB carve-out. C is defensible if a maintainer makes the written case; default is still Rust.
- Pull request reviews don't ask "should this be in C instead?" — the choice is settled — except for the smallest-TCB cases, where the question has a documented answer.
- Smoke tests / scratch validation hitting raw FreeBSD APIs (jail_set, cap_enter, etc.) are pragmatically C if it's faster to write — see `scratch/jail-smoke/`. Production code is Rust.

## Honest take on the FreeBSD-community question

This was the question that prompted the 2026-05-07 revision: "won't
Rust create friction with FreeBSD-native contributors?" The honest
answer:

- **Atrium's contributor pool isn't core-FreeBSD-committers.**
  Atrium is a platform layered on FreeBSD, like X.org / Wayland /
  KDE / GNOME are layered on Linux. Wayland's Weston is C; smithay
  is Rust; both have communities. The Linux kernel community
  didn't write Weston, and core FreeBSD developers won't be the
  primary Atrium contributors either.
- **A C-preferring contributor still has a clear path in.** Per
  §"Why C at the boundaries", such a contributor can: write a
  Fresco client in C, write a system service speaking aqueduct in
  C, author a kernel driver targeting the Atrium GPU ABI in C, or
  build a different desktop on top of Fresco in C. The "you can't
  write a privileged Atrium daemon in C" friction applies only to
  *our reference implementation* of those daemons.
- **Don't optimise for hypothetical contributors who haven't
  materialised yet.** Optimise for "the code I write today is
  correct and safe", because that's what attracts contributors
  long-term. The Rust safety story for privileged daemons is the
  single biggest correctness lever we have.

Picking Rust does mean some FreeBSD-native developers won't
contribute to the daemons. We accept that trade-off. We don't
accept the trade-off of writing privileged input parsers in
hand-rolled C.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — overall platform thesis.
- [NAMING.md](NAMING.md) — component vocabulary.
- [ORGANIZATION.md](ORGANIZATION.md) — repo layout, including how Rust crates and C headers coexist within each repo.
