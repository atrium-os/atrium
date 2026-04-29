# Language policy

> **Kernel = C. Userspace = Rust. Public APIs = C ABI.**

This document records a one-time architectural decision so contributors know the rule and the reasoning behind it. Re-litigating the question costs more than it saves; that's the only reason this document exists.

## The rule

| Layer | Language | Why |
|---|---|---|
| Kernel modules (`atrium-kmod`, future GPU/display/Tessera drivers) | **C** | Required — FreeBSD kernel doesn't accept Rust |
| Public ABIs (`libfresco`, `libatrium-gpu`, `libcastellum`, ...) | **C headers** | Universal consumption — vendors, contributors, other languages |
| Rust binding crates (`fresco-rs`, `atrium-gpu-rs`, ...) | Rust | Ergonomic safe wrappers over the C ABIs |
| Display server (`fresco-server`) | **Rust** | Multi-client, multi-threaded, performance-critical, security-critical |
| Privileged userspace daemons (`portcullisd`, `castellumd`, `vestibulumd`, `lyrad`, `tabulad`, `praecod`, `opifexd`, `curiad`, `scriniumd`) | **Rust** | Same reasons as the server, doubly so for jail launcher / IPC bus |
| Shell + foundation apps (`forum`, `atrium-edit`, `atrium-term`, `atrium-files`, `atrium-image`, `atrium-pdf`, `atrium-clock`) | **Rust** | Already built; ecosystem alignment with Slint / Servo |
| Wire-format and ABI specifications | **Language-agnostic** | A conformant C, Go, Zig, or Swift impl is welcome |

## Why Rust for userspace

1. **Memory safety in a 1000-jailed-app world.** Atrium's thesis is "every app is its own jail." That model requires the privileged daemons to be unimpeachable — UAF, double-free, and buffer overflow in `portcullisd` or `fresco-server` would defeat the whole architecture. The bug classes Rust eliminates are exactly the classes that have plagued X servers and Wayland compositors for decades.
2. **Concurrency without data races.** The display server has per-slot rings, multi-client fan-in, per-window FBOs, and shared content-addressed stores. `Send`/`Sync` enforcement is a real correctness tool, not a luxury — it's how we get away with doing the right thing rather than the cheap thing.
3. **Ecosystem alignment.** Slint (D5) and Servo + WebRender (D6) are Rust. Modern text and graphics tooling — `lyon`, `rustybuzz`, `swash`, `tiny-skia`, `cosmic-text` — is Rust-first. Asahi-style GPU reverse engineering is Rust-first. Picking C means re-implementing that stack ourselves.
4. **Already shipped.** `fresco-server`, the four foundation apps, `fresco-rs`, `fresco-text`, and `atrium-gpu-rs` are Rust. Switching is enormous cost for arguable gain.
5. **Industry direction.** The Linux kernel accepts Rust. Microsoft is shipping Rust in Windows. Google ships Rust in Android. Betting on Rust as a systems language in 2026 is no longer speculative.

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

## What this is NOT

- **Atrium is not a "Rust OS."** It's not Redox. The kernel is FreeBSD; large parts of base are C; the ports tree is mostly C. We are not on a mission to rewrite anything that already works.
- **Atrium does not require C contributors to learn Rust.** A C developer can ship a kmod, a Fresco client, or a Castellum service against the public ABIs. The Latin-named privileged daemons are written in Rust because *we* wrote them in Rust; that's not a barrier to entry for someone wanting to add a different daemon to the platform.
- **Atrium does not freeze on Rust forever.** If a future Rust ABI stability story or a different language eats Rust's lunch, this doc gets revised. The current decision is right for the current decade.

## Practical consequences

- New service from scratch → Rust crate, vendored deps via `cargo`, published as a port that wraps the cargo build.
- New kernel driver → C source under `atrium-kmod/`, `bsd.kmod.mk` Makefile.
- New public API → C header under each repo's `include/`, with a Rust binding crate alongside (`<service>-rs`).
- Pull request reviews don't ask "should this be in C instead?" The choice is settled.

## See also

- [ARCHITECTURE.md](ARCHITECTURE.md) — overall platform thesis.
- [NAMING.md](NAMING.md) — component vocabulary.
- [ORGANIZATION.md](ORGANIZATION.md) — repo layout, including how Rust crates and C headers coexist within each repo.
