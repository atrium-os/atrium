# Portability doctrine — port the architecture, not the OS

Status: doctrine (settled 2026-06-17). Anchors how Atrium's core technologies relate
to other operating systems, so the question isn't re-litigated as an all-or-nothing
choice between "niche" and "betray the clean-cut."

> **One sentence.** Atrium-the-OS stays a clean-cut FreeBSD-native reference
> implementation; Atrium-the-*architecture* travels as open protocols + a portable
> userspace behind a thin OS seam — others implement the substrate on their own OS,
> we don't maintain N backends.

## 1. The tension

Atrium's value is the clean-cut: no linuxkpi, no X11/Wayland cruft, no JS-runtime
browser, a capability-jailed app model. That clean-cut is only possible *because*
we don't have to stay compatible with everyone's legacy. But the corollary is real:
a FreeBSD-only stack is **niche** regardless of how good it is — and if the core
tech (Fresco, Aqueduct, Pergola, the bundle/app model) isn't reachable from other
OSes, other OSes won't adopt it.

The trap is treating this as binary. It isn't. "Portable" fractures into three
layers with completely different answers.

## 2. The three layers

| layer | examples | portable? | how |
|-------|----------|-----------|-----|
| **Protocols + formats** | Fresco display protocol; Aqueduct/Castellum envelope + classes; scene-graph ops; bundle format (`.insula`/`atrium.toml`); `atrium-app://` naming; capability-manifest schema | **Yes — trivially** | publish the spec + a permissive reference impl; anyone implements a conformant client/server on any OS |
| **Rust userspace libraries** | `fresco-client`, `libatrium`, Pergola, the serializers | **Yes — behind a thin OS seam** | substrate-agnostic Rust; no FreeBSD calls leaking in; cheap if the discipline holds |
| **OS substrate** | Portcullis jails, Capsicum, the kqueue multiplexer, the GPU/display kmods, devfs | **No — and shouldn't be** | it *is* the OS; another OS implements the seam against its own primitives (namespaces/seccomp/Landlock, DRM, epoll) — that's *their* port, not our burden |

## 3. The cure for niche is NOT porting the OS

Making the OS run on Linux is the wrong cure, for three reasons:

1. **It dilutes the thesis.** Accommodating another OS's primitives drags back
   lowest-common-denominator abstractions and, eventually, the baggage we fled. The
   clean-cut works *because* we don't have to be compatible with everything.
2. **We already concluded this** when the macOS Insula host adapter was retired
   (2026-06-17, `atrium-bundle-format.md` §7): *maintaining* a substrate port is a
   standing tax with no payoff yet. Atrium-native is the path.
3. **The strongest property is the least portable.** The jail-as-sandbox — what
   enables the no-JS browser, native-untrusted-code, and location transparency
   (`atrium-navigator.md`) — is FreeBSD jails + Capsicum. On Linux's
   namespaces+Landlock it's a *weaker* sandbox. The thing that makes Atrium special
   is exactly the part that doesn't travel.

The cure for niche is to spread the **architecture**: open protocols (others
implement on their substrate) + a portable userspace + the apps ecosystem
(toolkit-backends + source-ports, `toolkit-backends.md`). The burden inverts —
portability becomes "publish + keep the seam clean," not "maintain N backends."
This is how the web, Wayland, and USB spread: a spec + a reference impl, not "port
our kernel." It's also what the roadmap already gestures at ("design choices flow
back into other ecosystems by example"; year-5 standardization).

## 4. We're already building at the right layer

Portability-at-the-seam has been happening without the name:

- **CBOR at boundary wires** (the postcard/CBOR decision, `aqueduct.md`) — an open,
  any-language, deterministic format *is* the portable wire substrate.
- **libatrium as a C ABI** — any FFI-capable language, on any OS that runs the shim,
  reaches the wire without seeing it.
- **Fresco** = protocol + `fresco-client` (portable) + a **backend-abstracted**
  server (Tier-2 SW / Carillon / native GPU are already a seam); a Linux frescod
  would be another backend behind it.
- **Aqueduct** = envelope + classes (portable spec) over a thin **kqueue transport
  seam** (epoll elsewhere).

The genuinely FreeBSD-bound piece is **Portcullis/jails/Capsicum** — and that is
correctly kept native.

## 5. The discipline (what this doctrine asks of every new decision)

1. **The public, portable surface = protocols, formats, and seams.** Spec them
   openly (CBOR/CDDL where cross-boundary; documented wire layouts). A third party
   must be able to implement against the *spec*, never by reading our Rust.
2. **App-facing libraries stay substrate-agnostic.** `libatrium`, `fresco-client`,
   Pergola must not leak kqueue/jail/Capsicum/cdev specifics. OS-specifics live in
   the **daemons**, behind a seam.
3. **OS-specifics behind a named seam, not scattered.** The transport, the
   sandbox-launch, the GPU/display backend are *interfaces*; the FreeBSD
   implementation is one impl of each.
4. **Do NOT build OS host-backends now.** No Linux/macOS runtime port. That is a
   later, *others-can-do-it* thing, enabled by #1–#3 — not a maintenance burden we
   take on pre-emptively.

These are low-regret constraints: they make the FreeBSD code *cleaner* and keep the
bigger stage reachable, at the cost of a little spec/seam hygiene.

## 6. Honest end-state

- **FreeBSD-Atrium = the reference implementation and the gold-standard sandbox.**
- **The model + protocols travel.** A Linux "Atrium" (if anyone builds it) is a
  *weaker-sandboxed implementation of the same architecture*, against the published
  seams — built and maintained by whoever wants it, not by us.
- **Niche-escape comes from influence + apps + protocol adoption**, not from
  being-everywhere-as-a-product. Even the Plan 9 outcome (ideas influence everyone,
  modest market share) is a real win; the protocol + ecosystem path maximizes actual
  adoption while leaving the clean-cut intact.

## 7. Relationship to other specs

- `atrium-bundle-format.md` §7 — the retired macOS host adapter (why substrate ports
  are not our burden).
- `aqueduct.md` — the postcard (internal) vs CBOR (open/boundary) wire rule; the
  three-layer transport/serialization/RPC separation.
- `toolkit-backends.md`, `atrium-navigator.md` — the apps-ecosystem and
  architecture-as-spec plays that drive niche-escape.
