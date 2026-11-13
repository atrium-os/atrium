# GPU security model: who touches the GPU, what confines them, and W^X

> **Status:** write-up, 2026-11-13, after stages 3 to 5e step 1 passed on the R9700
> (`docs/spec/r9700-bare-metal.md` §5, main at `eddf5f9b`). It collects what the GPU side
> relies on today from [`gpu-isolation.md`](gpu-isolation.md), [GPU ABI v2.2](atrium-gpu-abi-v2.md)
> (§3 principles 11 and 13, §4.1, §5.9, §5.11, §10, §10.5),
> [`gpu-app-path-f6.md`](gpu-app-path-f6.md), [`tier2-robust-access.md`](tier2-robust-access.md),
> [`gpu-power-management.md`](gpu-power-management.md), [`../GRAPHICS-STACK.md`](../GRAPHICS-STACK.md),
> [`kernel-hardening.md`](kernel-hardening.md) and `atrium-gpu-amd/ISOLATION.md`, checked
> against the code, and names the gaps. It is a map, not a new rule: where it says
> "decide", nothing is decided yet. Where a spec and the code differ, this file says what
> the code does and §9 lists the spec text that is out of date.

## 1. The short version

- **No app holds a GPU fd in production.** Apps reach the GPU only through frescod
  (scenes, or F6 code plus closed-set records) or through `aqueduct-gpu-host` (Vulkan via
  `atrium-vk-icd`, which today runs on the CPU, not the GPU). Jails and devfs rulesets keep
  apps off the device nodes, and `GpuDrivers::grant_check` refuses a direct grant because
  `atrium-gpu-amd` is still `experimental`.
- **Jails decide who reaches the driver; they cannot confine DMA.** Isolation between GPU
  clients is the driver's and the GPU MMU's job: each VM fd has its own VMID and
  kernel-built page tables. This is proven on silicon (5b to 5d): cross-VM writes fault and
  leave the target untouched, revoked pages fault, a faulting or hung queue is lost alone,
  and freed VRAM is scrubbed before anyone else gets it.
- **The kernel writes everything privileged:** page tables, queue descriptors (MQDs),
  the hand-off to MES, the queue's ring, preemption, reset and power requests. A client
  supplies at most an indirect buffer (IB) of PM4 that runs unprivileged under its own VMID
  (§5). On the F6 app path even that is written by the broker, never the app.
- **Firmware is in the TCB.** PSP, SMU, MES and RLC run AMD-signed images that the PSP
  checks; the driver only parses their container headers (bounds-checked Rust) and never
  looks inside. Power requests go through a fixed allowlist that never touches limits,
  voltage, fan or overdrive (§6).
- **W^X holds on the CPU** (`kern.elf64.allow_wx=0`; queue-fd mappings refuse
  `PROT_EXEC`; generated shader code is mapped RW, then flipped to RX, never both).
  **On the GPU it does not yet:** client data BOs are mapped readable, writable *and*
  executable (§7.2). That stays inside the client's own VM, so it is not an isolation
  break, but it is a gap worth closing.

## 2. Who touches what

| Process | Runs as | Reaches | Holds | Confined by |
|---|---|---|---|---|
| **App** | per-app uid, own jail (devfs ruleset 22 or baseline) | frescod's scene socket; `FRESCOD_GPU_SOCK` only if the TCB mounts it (`gpu.access`, F6); the aqueduct-gpu broker socket if the manifest says `graphics = "vulkan"` | no GPU fd, no device node | jail + devfs ruleset; F6: at most 4 GPU contexts per uid; 32 Fresco connections per uid |
| **Per-client renderer** (`fresco-renderer`) | `_fresco-render` (65011), one-shot jail per client connection, launched by portcullisd (frescod fails closed without it) | frescod over SEQPACKET plus a memfd slot sealed against resize | no GPU fd, no device node, no network, no host files; precompiled Tier-2 kernels from its bundle (it never compiles or writes code) | jail, 512 MiB / 100 % CPU (racct); frescod trusts only the frame header |
| **frescod** (the GPU broker) | `_frescod` (65010), jail `atrium-frescod`, non-root, `children_max = 0`, no network | `/dev/atrium-gpu0` (client node) and `/dev/atrium-display0`, through devfs ruleset 23, which jaild pins to this jail, binary and uid | VM, BO, queue and syncobj fds for every window and every F6 context; the display node, behind the display lease | jail; per-uid BO caps (it is a separate uid from the renderers); the kernel's per-VM isolation bounds what a compromised frescod can reach in other VMs, but it holds all of them |
| **aqueduct-gpu broker** | `_aqueduct-gpu` (65014), own jail, `devfs_ruleset = 0` (jaild mounts no devfs at all) | its socket; portcullisd (ExecInstance) | no GPU fd, no device node | jail; at most 64 workers, 16 per client uid; fails closed without portcullisd |
| **aqueduct-gpu worker** (per app connection) | `_aqueduct-gpu-app` (65015), one-shot jail of `org.atrium.aqueduct.gpu` | fd 0 (its one session) | no GPU fd, no network, no host files; runs the app's SPIR-V as native CPU code (Tier-2) | jail, 1 GiB rctl cap; the compiler it spawns runs in Capsicum capability mode (`cap_enter` before reading input), RLIMIT_CPU 30 s, RLIMIT_AS 4 GiB; bounds-checked SSBO access (`tier2-robust-access.md` §2) |
| **Control node** `/dev/atrium-gpu0ctl` | root, 0600 on the host | GPU reset, scheduler weights, power gating | none | no jail devfs ruleset unhides it (ISOLATION.md break 5) |
| **TCB** | jid 0, root | everything | | only jaild, portcullisd and bootstrap run as unjailed root |
| **`atrium-gpu-amd`** | kernel: C hardware backend, Rust client layer (`rs/`) | the hardware | page tables, MQDs, IH ring, VRAM allocator and scrub engine | ioctl validation in Rust (`rs/ioctl.rs`: every argument treated as hostile); fuzz gate (ABI §10.5, not yet run); attestation (`gpu-isolation.md`) |
| **PSP, SMU, MES, RLC firmware** | on the GPU | the GPU | | AMD's signing, checked by the PSP; SCPM signs the power table (we cannot change them; they are in the TCB) |

**Direct client fds** for an app exist only in test and dev setups, behind the
attestation gate (`GpuDrivers::grant_check` in `portcullis/jaild-policy`: right driver,
`production` status, a green isolation test bound to the live `build_id`, never a
`-dirty` or `unknown` build). `atrium-gpu-amd`'s stanza is `experimental`, and nothing in
the tree calls `grant_check` outside its tests, so no path grants `gpu` to an app today.

**The display** is frescod's alone, in three layers: the node is 0600 root on the host;
only ruleset 23 shows it, and jaild mounts 23 only for jail `atrium-frescod` running
`/usr/local/bin/atrium-frescod` as 65010 with no child jails; and the driver's open admits
only the jail and uid named in `dev.atrium_gpu_amd_display.0.lease`, which jaild writes
before frescod starts (not writable from a jail). Root on the host is refused too while a
lease is set. A commit names a BO by fd, and the kernel refuses a pitch × height larger
than the BO.

## 3. The kernel boundary, as built

ABI 2.1 §4.1 specifies a broker-held, non-passable `control_fd` that mints client
`device_fd`s carrying a jail id, quota and flags (`CLIENT_OPEN`, `SET_QUOTA`). **The driver
does not implement that yet.** What it has instead:

| ABI 2.1 asks for | `atrium-gpu-amd` today |
|---|---|
| control vs client authority on one node, checked on the context | two nodes: `/dev/atrium-gpu0` (client, 0600 root on the host, 0666 inside rulesets 21 and 23) and `/dev/atrium-gpu0ctl` (reset, sched, powergate; 0600 root, never in a jail). Control ioctls on the client node return `EPERM` |
| control fd not passable | the ctl node's ioctls are not checked per call, so a root-opened ctl fd passed over `SCM_RIGHTS` keeps its authority. Only root can open it, so this is defence in depth |
| per-jail quota record from `CLIENT_OPEN` | per-**uid** caps on VRAM and System BOs (`rs/quota.rs`; 75 % of VRAM, 50 % of RAM, `EDQUOT`), charged before allocation and given back at the real free. No per-jail record, no VA, VM, syncobj or in-flight caps; no cap on live queues per uid yet (the r9700 §4.5.1 plan asks for one) |
| `ALLOW_USER_QUEUES` on the minted client | `QUEUE_CREATE` with `USER_MODE` requires `priv_check(PRIV_DRIVER)`: host root only, refused to jailed root (`rs/uqueue.rs`, interim) |
| `BO_SEAL` (§5.13) | not implemented; read-only binds exist (`VM_BIND2` + `BIND_READONLY`, ABI rev 3) |

What the kernel does enforce, in the Rust client layer:

- **Capabilities are fds.** VM, BO, queue and syncobj are fds, so the Capsicum rights and
  `SCM_RIGHTS` rules apply. A queue fd is the right to submit to one queue in one VM, and
  holds a reference on that VM so its page tables outlive it (5c-2b test 4: a child that
  received the fd submits; after the last close the queue is gone). The last close cancels
  owed completions (`ECANCELED`), revokes every mapping, then removes the queue from MES.
  If MES will not let go, the VM reference is leaked on purpose so page tables are never
  freed under a queue that may still run.
- **Every struct and fd is checked** (`struct_size`, zero padding, unknown flags, fd kind,
  VA alignment and overlap, IB size up to 0xfffff dwords, IB alignment, VMID range).
- **Memory lifetime (§5.11).** A BO unbind clears PTEs and flushes the VMID; the backing is
  released only after MES has preempted every queue of that VM once, never on a fence value
  a client can write (5c-2b step 4). Charges stay with the uid until the real free.
- **Jail death does not revoke.** As ABI §4.2 says, contexts live until their fds close;
  process exit closes them.

## 4. GPU-side isolation: what is proven on the R9700

| Property | Mechanism | Silicon evidence |
|---|---|---|
| Per-client address space (I1) | each VM fd has a kernel VMID (1 to 7) and its own gfx12 four-level page table; MES user queues run under VMIDs 8 to 15 that MES assigns, but the page-table base it is given is the kernel's. Clients never write a PTE | 5b: VMID 1 runs write_42 through its own table |
| No cross-client access (I2, I3) | another VM's VA misses in this VM's table: UTCL2 fault, the write is dropped | 5b test B; 5c-2a: the cross-VM write faulted, failed only that submit (EIO), other VM's page untouched |
| Revocation | unmap + per-VMID TLB flush with ack | 5b test C: revoked page faults, target untouched |
| A fault or hang costs one queue | fault IV names the VMID: that queue is lost (EIO, later submits refused), waves killed, HQD reset, REMOVE_QUEUE; hang watchdog at `hang_ms` (5 s) | 5c-2b steps 2 and 3: fault, kernel-mode hang (5.04 s) and user-ring hang (5.25 s) each lost alone; other VMs run on |
| Same VAs, separate pages | per-process VM plus MES process context | 5c-1, 5c-2: two VMs at identical VAs see only their own pages |
| No privileged state from clients (I4) | MQDs, VMIDs, page tables, doorbell routing and the MES API are kernel-only; client queues are MES user queues with no `PRIV_STATE` or `KMD_QUEUE` in their MQD; the kernel-mediated ring is read-only in the client's VM | 5c-2b step 3: user-mode refusals hold. **Not yet tested:** a client IB that writes protected registers (§5) |
| Memory reuse after close (I5) | free rule above | 5c-2b step 4: 21 to 26 preemptions, 0 failed, nothing deferred |
| VRAM never handed out dirty | all VRAM starts dirty; a CP `DMA_DATA` zero fill on the kernel-only queue under VMID 0 (which no client VMID maps) scrubs every freed range before it returns to the arena; a failed scrub keeps the range back; the PSP's reservations stay outside the allocator | 5d: 31980 MiB scrubbed in 410 ms; a freed BO's next owner reads zeros |
| No CPU access to VRAM BOs | CPU read refused; the CPU never writes VRAM | 5d test 11 |
| Doorbell is one queue | one queue per doorbell page; mapped only through that queue fd's managed pager (`RamPages`, KPI patch 0012); revoked on close, loss or reset | 5c-2b test 9: after close, ring and doorbell SIGBUS in a child |

Still `experimental`: `test_gpu_isolation` itself (the gpusim suite, being moved to queue
fds per the 2026-11-13 decision) and the F6 hostile-ISA run (G5) have not run on silicon,
and the 24 h fuzz gate has not run at an attested commit.

## 5. What the command processor runs, by path

The CP executes PM4. Who writes it decides how much a client can ask of the GPU.

| Path | Ring written by | Client-written PM4 | Holder |
|---|---|---|---|
| **frescod's own renderer** (`FRESCOD_RENDERER=gpu`) and **F6 app code** | frescod, through `atrium-gpu-app` / `atrium-rdna4-cmd` | none. The app sends a code header, machine code and closed-set records (bind code, user SGPRs, dispatch, barrier); the broker checks every field and writes every packet into a ring that is read-only to the GPU | frescod only |
| **Kernel-mediated queue fd** (`QUEUE_SUBMIT`, ABI §5.7) | the kernel: INDIRECT_BUFFER, then RELEASE_MEM (fence + EOP interrupt naming the queue's doorbell) | the IB contents, run by the CP under the queue's VMID on an unprivileged MES user queue | whoever holds the queue fd: frescod in production, root or a dev jail in tests |
| **User-mode queue** (`USER_MODE`, §5.9) | the client, via its mmap of the ring and doorbell | the whole ring | host root only (`PRIV_DRIVER`) |

So "the broker writes every packet" is true of the F6 path, not of queue fds in general. A
queue-fd holder controls the PM4 in its IB. Its limits are the VMID (every memory access,
including `WRITE_DATA` and `DMA_DATA` from the IB, is translated by the client's page
table), the unprivileged queue (no `PRIV_STATE`/`KMD_QUEUE`), and the watchdog. Register
writes from an unprivileged IB are expected to be refused by the CP's register protection,
as on amdgpu, where any render-node user submits IBs. That expectation has no test yet.

The fence a kernel-mediated queue writes lives in the client's own VM: a client can make
only its own syncobjs signal early, never another queue's, and never a free.

## 6. Firmware, PSP and power

- **Loading.** Images come from `/boot/firmware` through `firmware(9)` (root-owned).
  `rs/fw.rs` parses only the container headers, checking every offset and size against the
  file, pure Rust with no `unsafe`, host-tested against the real R9700 files. The PSP loads
  the sOS chain, then all IP microcode (`LOAD_IP_FW`, including MES, MEC, PFP and ME) and
  verifies the signatures. The driver reads the MES header only for its start address.
- **What the driver cannot see.** PSP, SMU and MES are closed, signed and in the TCB. MES
  picks VMIDs and HQDs for the process contexts the kernel gives it; a MES bug could break
  queue isolation, and nothing in the driver would catch it. The VMID's page-table base is
  still the kernel's, so a MES bug cannot hand a client someone else's page table unless it
  also mixes up contexts.
- **VRAM layout.** The PSP's reservations (50 MiB at the top and 4 MiB below) and the
  driver's own area are outside the client allocator's range; `si_map_page` refuses a VRAM
  address outside it.
- **Power (gpu-power-management.md §A3).** The driver can express only the messages in its
  allowlist enum, looked up per SMU generation; a generation without a row gets nothing.
  Never sent: feature disables, overdrive, PPTABLE/SKU table writes, manual fan, voltage
  offsets, hard minimum above default. Requests only lower limits. Any unexpected answer or
  timeout puts the link in a failed state: nothing more is sent and the firmware keeps
  control. Required protections (FW_DSTATE, THROTTLERS) must be running or the GPU is
  refused. A card the driver cannot prove idle by telemetry is reset to post-POST and
  refused, never left powered.
- **Bring-up switches** (`hw.atrium_gpu.psp_bootload`, `smu_init`, `gc_up_ok`, `client`,
  `dispatch` and the rest) are kenvs: root on the host only.
- **Last rung.** If MES refuses to remove a queue even after an HQD reset, the card gets a
  mode 1 reset and bus mastering is switched off for good (`amd_si_lose`): every client
  queue fails with EIO and the driver can detach. The device fails closed rather than
  running with a queue nobody controls.

## 7. W^X

### 7.1 CPU side

- **System-wide:** `kern.elf64.allow_wx=0` (`etc/sysctl.atrium.conf`, kernel-hardening.md
  §4): no mapping may be writable and executable at the same time. `nxstack` and ASLR on.
  This does not stop a process flipping its own pages RW → RX.
- **GPU queue mappings:** `fo_mmap` on a queue fd caps `maxprot` at read-write and refuses
  `PROT_EXEC` and `MAP_PRIVATE` (`glue.c`, `amd_rs_mmap_object`; 5c-2b test 9). BO contents
  move through `BO_READ`/`BO_WRITE` copies, with no CPU mapping of BOs.
- **Loaders:** `hospes-elf` refuses W+X segments and executable stacks.
- **Shader code generation.** Native code from SPIR-V appears in three places:
  - *aqueduct-gpu worker* (app SPIR-V, untrusted): compiled by `atrium-spv-compile` in
    Capsicum capability mode, returned over a pipe, mapped by `atrium-spv-loader`'s
    `jitmap` RW, copied, I-cache synced, then `mprotect` RX. The worker writes no files
    (empty cache root), so compiled code is per app. A compromised compiler can hand back
    arbitrary code; the per-app executor jail is the backstop, by design.
  - *fresco-renderer* (our kernels): precompiled into its bundle; the jail neither compiles
    nor writes code.
  - *frescod* (our kernels): uses the installed `/usr/local/share/atrium/fresco-kernels`
    if complete; otherwise it compiles them in-process with Cranelift into the system cache
    or a private 0700 directory, then maps them the same RW → RX way.
  `tier2-renderer.md` decided "no JIT in the daemon". The frescod fallback is runtime
  codegen in the long-lived broker; it never compiles app input, but **decide** whether to
  keep the fallback or make frescod refuse to start without installed kernels. Either way,
  set `PROT_MAX(PROT_READ|PROT_WRITE)` on anonymous mappings in processes that must never
  gain execute (frescod once kernels are loaded, the renderers), so RW → RX is impossible
  there rather than merely unused.
- **Shader cache trust.** Blobs on disk are keyed by the hash of their SPIR-V *input* and
  mapped executable with no check of the blob itself, and `seed_cache` skips a blob that
  already exists. Whoever can write a cache directory a process loads from can run code in
  that process. Today the directories are frescod's own or read-only mounts, so this holds
  by file permissions alone. Keep `/var/atrium/shaders` writable by no one but its single
  user, or sign/hash the output.

### 7.2 GPU side

The gfx12 PTE has separate READABLE, WRITEABLE and EXECUTABLE bits (`gmc12.c:51`).

- **Code is read-only:** `VM_BIND2` with `ATRIUM_GPU_BIND_READONLY` maps without write
  (ABI rev 3); frescod maps F6 app code and constants, and its own kernels, that way. The
  kernel-mediated ring is read-only and executable in the client's VM (the CP's ring fetch
  needs X: the first 5c-2b run faulted with `PERMISSION_FAULTS 0x8` without it).
- **Data is executable too (gap).** Client BO mappings on silicon always carry EXECUTABLE:
  `si_client.c:1135` uses `READ | EXEC (| WRITE)`, the user-queue writeback and EOP pages
  are RWX in the client VM (`mes12.c:2528`), the stage-5c-1 test VMs are RWX
  (`mes12.c:1359`, `:2049`), and GART pages are RWX (`gmc12.c:387`). So a shader can write
  a buffer and branch into it. For app code on the F6 path this changes nothing (the app
  already supplies arbitrary ISA, and the VMID is the boundary). It matters for frescod's
  own kernels, which process untrusted scene data: a memory-safety bug in one would become
  code execution in that VM instead of a wrong pixel.
- **Proposal:** a bind flag for executable (or X only with `BIND_READONLY`), X on shader
  code and on what the CP fetches (rings, IBs); never X on data BOs a dispatch can write.
  Add a `test_gpu_isolation` case: a dispatch that writes code into a data BO and jumps to
  it must fault. Check first whether gfx12 treats IB fetches as execute accesses, since
  IBs live in client BOs.

## 8. What fails closed

- No portcullisd: frescod refuses clients (no unjailed renderer), and the aqueduct-gpu
  broker refuses clients (no unjailed worker).
- A devfs ruleset id the kernel has not loaded: jaild refuses to create the jail.
- A pinned ruleset (23 display, 24 audio, 26 input) requested by anything but its service:
  refused.
- A changed or dirty driver build: the attestation no longer matches, `gpu` is refused.
- An unknown SMU generation or metrics layout: read-only, no messages, no decoded
  telemetry. An unknown GC version: MES not touched.
- A scrub that fails: VRAM held back, retried, never handed out. No clear engine: freed
  VRAM is never reused.
- A queue MES will not remove: mode 1 reset, bus mastering off, all queues EIO.

## 9. Spec text that no longer matches the code

The coordinator asked about an older Mesa plan. `GRAPHICS-STACK.md` §10 is now correct (no
Mesa at runtime on the default path; lavapipe only as a test oracle and behind
`FRESCOD_RENDERER=vulkan`; the `atrium-os/mesa` fork only ever carried venus). The stale
text is elsewhere:

| Where | Says | Now |
|---|---|---|
| `gpu-isolation.md` intro and §Default capability classes | an app with `gpu` "talks directly to the GPU through Mesa"; `gpu-direct` is an opt-in app class | the 2026-11-03 amendment overrides it (test/dev only), but the body still reads as if direct grants were normal; the RDNA4 backend is ours, no Mesa (ABI §13p) |
| `gpu-isolation.md` driver table | separate `atrium-amdgpu` row "user shaders go through MEC user queues", spec-only | that is `atrium-gpu-amd`, on MES user queues, on silicon now |
| `atrium-gpu-abi-v2.md` §1, §2 | Mesa winsys port as the plan | §13p and §11 say our own backend first, radv winsys a fallback; the intro was not updated |
| `atrium-gpu-abi-v2.md` §18 | status "as of 2026-11-03": none of 2.1 implemented | most of the table is done (ISOLATION.md); the open rows are `CLIENT_OPEN`/control fd, per-jail quota, `BO_SEAL`, cross-device share (§3 above) |
| `aqueduct-gpu.md` §12.3 | the app's ICD opens `/dev/atrium-gpu0` inside its jail and mmaps regions | apps hold no GPU fd; Vulkan goes to per-app executor jails over a socket (`tier2-robust-access.md` §4) |
| `GRAPHICS-STACK.md` §11 | "No silicon yet"; "no `QUEUE_CREATE`/queue fds" | stages 3 to 5e on the R9700; queue fds on MES user queues since 5c-2b |
| `etc/jaild.policy.toml` | `atrium-virtio-gpu` stanza `production` with commit `0000000` | `grant_check` refuses it (no real commit); the legacy driver should go to `experimental` or be removed |
| `si_client.c` header | "User-mode queues ... are 5c-2b: EOPNOTSUPP here" | implemented in 5c-2b step 3 (comment only; left for the driver thread) |

## 10. Open items, by weight

1. **Hostile PM4 on silicon.** Add to the queue-fd suite: an IB that writes protected
   registers (VM context, IH, MES, SMU mailbox ranges) and one with `PRIV`-style bits set;
   each must fault or be ignored, with the device and other queues fine (§5).
2. **Run `test_gpu_isolation` and the F6 hostile-ISA test (G5) on the R9700**, then the
   24 h fuzz gate; only then may the stanza say `production`. Each box run needs Shankar's
   go.
3. **GPU W^X for data BOs** (§7.2), and `PROT_MAX` plus the frescod codegen fallback
   decision on the CPU (§7.1).
4. **IOMMU.** ISOLATION.md calls it "a second line" on silicon, and the x86_64-port plan
   names VT-d (`hw.dmar.enable=1`). Nothing in the tree shows DMA remapping enabled on the
   bare-metal Atrium install. Confirm it, and what it covers (the driver already uses
   `bus_dma` addresses, so it is DMAR-ready).
5. **Authority split per ABI §4.1**: `CLIENT_OPEN`, a non-passable control fd, per-jail
   quotas including live queues, VMs and syncobjs. Until then frescod is the only client
   in production and per-uid caps stand in.
6. **User-mode queues and "the broker writes every packet".** A user-mode queue's holder
   writes its own ring. Fine for root and frescod's own trusted renderer later; never for
   an app or a per-client renderer without a decision. Today it is host root only. Write
   the rule into GRAPHICS-STACK and ABI §5.9 when the interim `PRIV_DRIVER` gate changes.
7. **Devfs ruleset 21** (`atrium_gpu`: client node 0666, no display) is allowed but not
   pinned to any service, and no service uses it now. Pin it or drop it from
   `allowed_ids`, so only frescod's jail can ever see the GPU node.
8. **Firmware in the TCB** (§6): say so in the Navigator threat model's GPU rows.
9. **RamPages review** (KPI patch 0012): the non-contiguous path, `VPO_UNMANAGED` on
   object-less pages against vm_page invariants, a leak counter.
10. **No kernel KASLR** on FreeBSD (kernel-hardening.md §5): an info leak through the GPU
    ioctls would give the kernel layout. The fuzz gate is the defence.
11. **Display path** (gpu-isolation.md open question 1): frescod scans out app-derived
    pixels; the import and blit path must be memory-safe. Display ABI v2 already bounds
    scanout by the BO (break 4).
12. **Stale spec text** in §9.
