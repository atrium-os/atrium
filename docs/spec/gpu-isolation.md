# GPU isolation — requirements for `gpu` capability grant

**Status:** spec, 2026-05-07
**Owner:** Atrium platform / atrium-kmod

The `gpu` capability in `atrium.toml` is the most dangerous one Portcullis
can grant. An app with the `gpu` capability has `/dev/atrium-gpu0`
mounted into its jail and can talk directly to the GPU through Mesa.
Without proper isolation, the GPU can DMA to arbitrary host memory
— escaping the jail, bypassing kernel page protections, reading other
apps' BOs, and reading kernel-private data.

This doc nails down what isolation the kernel GPU driver must enforce
before Portcullis will grant `gpu` to any app, and how that requirement
is validated.

## Threat model

A direct-GPU app can:

1. Submit arbitrary GPU command buffers (vertex shaders, compute
   shaders, blits, copies).
2. Reference BOs by handle in those command buffers.
3. Implicitly issue DMA reads/writes via shader load/store, image
   sampling, transfer-buffer copies.
4. Program GPU MMU/IOMMU descriptors via privileged commands (if the
   GPU exposes them, which it should not from userspace command
   buffers — but proof of this is the kernel driver's job).

The attacker's goal: read or write memory outside their own jail's
allotted GPU BOs. This includes:

- Other apps' GPU BOs (e.g. another app's framebuffer, holding the
  user's password being typed)
- Kernel memory (page tables, credentials, fs cache)
- The host hypervisor's memory (on virtualised platforms)
- The GPU's own privileged registers (firmware, page tables)

## Required invariant

For every `open(2)` of `/dev/atrium-gpu0`, the kernel driver MUST
allocate an isolated GPU context with the following properties:

**I1.** *Per-context GPU virtual-address space.* All GPU virtual
addresses referenced by command buffers submitted on this fd resolve
through a page table dedicated to this context. The page table is
managed by the kernel driver; userspace cannot install or modify
mappings.

**I2.** *Per-context BO ownership.* A BO allocated through fd_A is
not addressable from any other fd. Specifically:

- Handle namespaces are per-fd; handle 7 in fd_A and handle 7 in fd_B
  refer to different BOs.
- Importing a BO across fds is permitted only via an explicit kernel-
  brokered channel (e.g. dmabuf-style fd-passing via `SCM_RIGHTS`
  followed by an `IOC_IMPORT_FD`-type ioctl that the kernel
  validates).
- Speculative reference by GPU VA — even if userspace guesses
  another context's GPU VA — must miss in the per-context page table
  and be rejected by the GPU (segfault-equivalent on the GPU side).

**I3.** *No DMA outside the context's BOs.* GPU DMA (shader loads,
sampling, transfer copies, scratch spills) on this context targets
only physical pages backing BOs allocated through this fd. The
hardware IOMMU (or its software equivalent) enforces this on every
transaction. A bug in user shader code cannot escape the BO set.

**I4.** *No privileged-command access.* User command buffers cannot
include commands that would modify the GPU's MMU descriptors,
firmware, or any other privileged hardware state. The kernel driver
either filters submitted commands or relies on a HW protection
boundary (e.g. user-mode vs kernel-mode command rings) to enforce
this.

**I5.** *Cleanup on context teardown.* On `close(fd)`, the kernel
driver tears down the context's page table, frees its BOs, and
ensures no residual mappings remain accessible to subsequent
contexts. (No data leak via reuse of physical pages without zeroing
GPU-VA mappings.)

## Per-driver-port responsibility

Each kernel driver that exposes `/dev/atrium-gpu0` is responsible
for proving the above for the hardware it drives. There is no
single Atrium-side enforcement; isolation is fundamentally a
property of the driver + hardware.

| Driver | Isolation provider | Status |
|--------|-------------------|--------|
| **`atrium-virtio-gpu`** (D0, current) | layered: kmod per-fd handle table + virtio-gpu spec context ownership + host venus per-VkInstance + Metal/Apple IOMMU on M4 Max | **Verified by smoke test** (see below) |
| **`atrium-asahi`** (D5+ Apple Silicon native) | Apple GPU per-context page tables + AGX MMU; kernel driver manages page tables; user shaders go through the AGX user command ring | **Spec-only; gated on D5 driver landing** |
| **`atrium-amdgpu`** (D5+ AMD discrete) | AMDGPU MMU per-VMID; kernel driver manages VMIDs and page tables; user shaders go through MEC (Micro Engine Compute) user queues | **Spec-only; gated on D5+ driver landing** |
| **`atrium-i915`** (D5+ Intel) | Intel PPGTT (per-process graphics translation tables); kernel manages PPGTT; user shaders go through user-mode command rings | **Spec-only; gated on D5+ driver landing** |

For each driver port, the team must:

1. Write a doc-section under `atrium-kmod/<port>/ISOLATION.md` mapping
   each invariant I1–I5 to the specific HW/driver mechanism that
   enforces it.
2. Implement the smoke test (see §Validation) for that driver.
3. Land the smoke test as a regression in `atrium-kmod/test_gpu_isolation.c`.
4. Only then can `portcullisd` enable the `gpu` capability for that
   hardware.

## Validation: `test_gpu_isolation`

A minimal isolation test, run as a regression on every `atrium-kmod`
build, exercising the cross-fd scenarios that should fail:

```
1. open two fds: fd_A, fd_B; IOC_CTX_INIT on each (ctx_A, ctx_B)
2. fd_A: IOC_BO_CREATE (handle h_A), fill with sentinel pattern
3. fd_B: try IOC_BO_REF_BY_HANDLE(h_A)            → expect ENOENT
4. fd_B: try IOC_BO_REF_BY_GPU_VA(known_A_va)     → expect ENOENT/EFAULT
5. fd_B: submit a command buffer that reads from gpu_va
        guessed within fd_A's BO range            → expect submission
                                                    rejection OR
                                                    GPU page fault
                                                    (driver-defined),
                                                    NOT a successful read
6. fd_B: submit a command buffer with fd_B-local VAs that point
        into fd_A's pages via clever shader math  → expect IOMMU fault,
                                                    NOT success
7. close(fd_A); fd_C: open + CTX_INIT + alloc fresh BO; verify the
   physical pages do not contain fd_A's sentinel pattern (i.e. the
   driver zeros pages or does not reuse them with stale GPU
   mappings)
```

Each step has a clear pass/fail. The test is parametric on the
underlying driver port — same C source, runs against whatever
`/dev/atrium-gpu0` is bound to.

For D0 (the current path), step 5 is enforced by the host venus
worker: the worker's MoltenVK instance is per-context, MoltenVK's
MTLDevice exposes only that context's MTLBuffers, and Metal's
IOMMU rejects DMA outside the MTLBuffer set. Step 6's "clever
shader math" can't reach because Metal doesn't expose raw physical
addresses to user shaders — Metal's GPU VA is sandboxed by Apple's
hardware IOMMU on M4 Max.

For D5+, step 5 / step 6 fall to the kernel driver's page-table
management, hardware IOMMU, and command-buffer validator (or HW
user ring boundary).

## Capability gate

Portcullis must refuse to grant the `gpu` capability if:

- The kernel driver bound to `/dev/atrium-gpu0` has no entry in
  `/etc/atrium/jaild.policy.toml`'s `[gpu_drivers.attested]` table; OR
- The attested entry's `isolation_test_passed` field is `false`; OR
- The kernel driver is in development (`status = "experimental"`).

This means: a freshly-bring-up kernel driver cannot accidentally
grant GPU access to an app. The platform engineer has to explicitly
flip the bit in the jaild policy file after running the isolation
test to green. This is the single switch that gates direct-GPU
deployment.

```toml
# /etc/atrium/jaild.policy.toml (excerpt)

[gpu_drivers.attested.atrium-virtio-gpu]
status                  = "production"           # production | experimental | broken
isolation_test_passed   = true
isolation_test_date     = "2026-05-07"           # last green run
isolation_test_commit   = "abc1234"              # atrium-kmod tree at the time
notes = "D0 path; isolation enforced by host MoltenVK + Apple IOMMU"

[gpu_drivers.attested.atrium-asahi]
status                  = "experimental"         # not yet ready for production
isolation_test_passed   = false                  # test does not yet exist
notes = "Pending D5; isolation work in progress"
```

## Default capability classes

For developer convenience, `atrium.toml` capability schema includes
two `gpu`-shaped capabilities, distinguished by trust level:

- **`render-only`** (default for GUI apps; always grantable): NO
  `/dev/atrium-gpu0`. App talks scene description over aqueduct;
  frescod renders for it. Suitable for editors, terminals, file
  managers, settings apps. Has no GPU-isolation dependency at all
  (the GPU is fully owned by frescod, which trusts the bundle SPV).

- **`gpu-direct`** (opt-in, prompted): `/dev/atrium-gpu0` mounted in
  the jail. Suitable for browsers, games, GPU compute, video
  editors. Refused unless the bound kernel driver is attested per
  the previous section.

Most apps will only need `render-only`. `gpu-direct` is the
exceptional case where an app has fundamental need for arbitrary
shader execution.

## Open questions / future work

1. **Display-side isolation.** Frescod has the scanout privilege.
   If frescod is compromised, can an attacker scan out memory pages
   it shouldn't see? Frescod's BOs come from its own `open()` of
   `/dev/atrium-gpu0`, so its context is isolated from apps' contexts.
   But frescod *displays* texture data uploaded by apps via
   aqueduct → texture import. The import path must be
   memory-safe (no use-after-free, no out-of-bounds blits). This is
   an implementation requirement for frescod, not for the kernel
   driver. Track separately.

2. **Multi-GPU systems** (D5+ servers). When there are multiple
   GPUs, each gets its own cdev (`/dev/atrium-gpu0`,
   `/dev/atrium-gpu1`, …). Per-GPU isolation is independent;
   `atrium.toml` `gpu-direct` capability can specify which GPU(s)
   the app may touch.

3. **Memory accounting.** A direct-GPU app can allocate large amounts
   of GPU memory and exhaust system resources for other apps. The
   kernel driver must honour `rctl(8)` / per-jail memory quotas.
   Track in `atrium-kmod` follow-up.

4. **Migration / suspend-resume.** When a jail is suspended (e.g.
   for live-migration in some hypothetical future), the GPU
   context's state must be saveable / restorable atomically. Out
   of scope for D5; flag for D7+ if migration ever lands.
