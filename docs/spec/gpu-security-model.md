# GPU security model: who touches the GPU, what confines them, and W^X

> **Status:** write-up, 2026-11-13, after stages 3–5e passed on the R9700
> (`docs/spec/r9700-bare-metal.md` §5). It collects what the GPU side relies on
> today from [`gpu-isolation.md`](gpu-isolation.md), [GPU ABI v2.2](atrium-gpu-abi-v2.md)
> (§3 principles 11 and 13, §4.1, §5.9, §5.11, §10, §10.5),
> [`gpu-app-path-f6.md`](gpu-app-path-f6.md), [`../GRAPHICS-STACK.md`](../GRAPHICS-STACK.md),
> [`kernel-hardening.md`](kernel-hardening.md) and `atrium-gpu-amd/ISOLATION.md`, and
> names the gaps. It is a map, not a new rule: where it says "decide", nothing is decided yet.

## 1. The short version

- **No app holds a GPU fd in production.** Apps reach the GPU only through
  frescod (scenes, or F6 code + closed records) or `aqueduct-gpu-host` (Vulkan via
  `atrium-vk-icd`). Jails and devfs rulesets keep them off the device nodes.
- **Isolation between GPU clients is the driver's and the GPU MMU's job, not the
  jail's.** A jail decides who reaches the driver; it cannot confine DMA. Each client
  gets its own GPU VM (VMID + kernel-built page tables). This is now proven on silicon:
  cross-VM writes fault and leave the target untouched, revoked pages fault, a faulting
  or hung queue is lost alone, and freed VRAM is scrubbed before reuse.
- **The kernel writes everything privileged:** page tables, queue descriptors (MQDs),
  the hand-off to the MES firmware, preemption, reset, power. Clients write only their
  own ring contents (user-mode queues) or nothing at all (kernel-mediated submit, and
  the F6 path, where the broker writes every PM4 packet).
- **W^X holds on the CPU** (`kern.elf64.allow_wx=0`; GPU queue mappings refuse
  `PROT_EXEC` and `MAP_PRIVATE`). **On the GPU it does not yet:** client data BOs are
  mapped readable, writable *and* executable (§5.2). That is contained to the client's
  own VM, but it is a gap worth closing.

## 2. Who touches what

| Process | Runs as | Reaches | Holds | Confined by |
|---|---|---|---|---|
| **App** | per-app uid, own jail | frescod's scene socket; `FRESCOD_GPU_SOCK` only with `gpu.access` (F6) | no GPU fd, no device node | jail + devfs ruleset; F6: ≤ 4 GPU contexts per uid |
| **Per-client renderer** (`fresco-renderer`) | `_fresco-render` (65011), one jail per client connection, launched by portcullisd | frescod over SEQPACKET + a sealed memfd slot | no GPU fd, no device node | jail, 512 MiB / 100 % CPU caps; frescod trusts only a header |
| **frescod broker** | `_frescod` (65010), jailed, non-root | `/dev/atrium-gpu0` (client authority) and `/dev/atrium-display0` | the VM, BO and queue fds for every window and F6 context; the display node | jail; the kernel's per-VM isolation bounds what a compromised broker can reach (gpu-isolation.md amendment) |
| **`aqueduct-gpu-host`** | broker for headless / third-party Vulkan | `/dev/atrium-gpu0` | client fds for its sessions | same as frescod |
| **Control node** `/dev/atrium-gpu0ctl` | root, mode 0600 | reset, scheduler weights, power gating, quotas, minting clients | — | no jail devfs ruleset unhides it (ISOLATION.md break 5) |
| **TCB** | jid 0, root | — | — | only jaild + portcullisd + bootstrap run as unjailed root (de-rooting, 2026-06-25) |
| **`atrium-gpu-amd`** | kernel (C + Rust in the module) | the hardware | page tables, MQDs, IH ring, VRAM allocator, scrub engine | ioctl hardening + fuzz gate (ABI §10.5); attestation (gpu-isolation.md) |
| **PSP / SMU / MES firmware** | on the GPU | the GPU | signed images loaded through the PSP; SCPM signs the power table | AMD's signing (we cannot change them; they are in the TCB) |

**Direct client fds** for an app exist only in test and dev jails, behind the
attestation gate (`GpuDrivers::grant_check`: right driver, `production` status,
green isolation test bound to the live `build_id`, never a `-dirty` build).
`atrium-gpu-amd` is `experimental`, so today the gate refuses `gpu` to every app.

## 3. The kernel boundary

- **Authority split (ABI §4.1).** Opening the device node is *control*; running work
  needs only *client* authority. The broker mints restricted client fds (carrying a jail
  id and quota) and may narrow them further with `cap_ioctls_limit`. User-mode queues
  need a client minted with `ALLOW_USER_QUEUES`.
- **Capabilities are fds.** VM, BO, queue and syncobj are fds. A queue fd passed over
  `SCM_RIGHTS` is a scoped grant to one queue in one VM (proven on silicon, 5c-2b
  test 4); its last close cancels outstanding work, revokes every mapping, and removes
  the queue from MES.
- **Attack surface hardening (ABI §10.5).** Every ioctl struct and the submit blob are
  fuzz targets; production status needs 24 h with zero new crashes at the attested
  commit.

## 4. GPU-side isolation: what is proven on the R9700

| Property | Mechanism | Silicon evidence |
|---|---|---|
| Per-client address space (I1) | each VM has a VMID and kernel-built gfx12 four-level page tables; clients never write a PTE | 5b: VMID 1 runs write_42 through its own table |
| No cross-client access (I2/I3) | another VM's VA misses in this VM's table → UTCL2 fault, write dropped | 5b test B: VMID 1 write to a page only VMID 0 maps faults, target untouched; 5c-2a test 3 |
| Revocation | unmap + TLB flush per VMID | 5b test C: revoked page faults, target untouched |
| A fault or hang costs one queue | fault → queue lost (EIO, later submits refused), waves killed, HQD reset, REMOVE_QUEUE; hang watchdog at `hang_ms` | 5c-2b steps 2–3: fault, kernel-mode hang (5.04 s) and user-ring hang (5.25 s) each lost alone; other VMs run on |
| Same VAs, separate pages | per-process VM + MES process context | 5c-1 and 5c-2: two VMs at identical VAs see only their own pages |
| No privileged commands (I4) | MQDs, VMIDs, page tables, doorbell routing and MES are kernel-only; the F6 path writes every packet in the broker; the user-mode ring is the client's own and runs under its VMID | 5c-2b step 3: user-mode refusals all hold |
| Memory reuse after close (I5) | BO frees wait for MES preemption of every queue that could reach it (free rule, ABI §5.11), never on a client-writable fence | 5c-2b step 4: 21–26 preemptions, 0 failed, nothing deferred |
| VRAM never handed out dirty | all VRAM dirty at attach; DMA-fill scrub on the kernel-only queue (HQD 2, VMID 0); generation-checked; failure leaves the range dirty | 5d: 31980 MiB scrubbed in 410 ms; a freed BO's next owner reads zeros |
| No CPU access to VRAM BOs | CPU read refused; the CPU never writes VRAM | 5d test 11 |
| Doorbell is one queue | one queue per doorbell page; mapped only from that queue's fd; revoked on close/reset | 5c-2b test 9: after close the ring and doorbell SIGBUS |

Still `experimental`: `test_gpu_isolation` and the F6 hostile-ISA run (G5) have not
run on silicon, and the 24 h fuzz gate has not run at an attested commit.

## 5. W^X

### 5.1 CPU side

- **System-wide:** `kern.elf64.allow_wx=0` (`etc/sysctl.atrium.conf`, kernel-hardening.md
  §4): no mapping may be writable and executable *at the same time*. `nxstack` and ASLR on.
- **GPU queue mappings:** `fo_mmap` on a queue fd refuses `PROT_EXEC` and `MAP_PRIVATE`
  (5c-2b test 9). The ring and writeback page are `RamPages` (KPI patch 0012), managed so
  revocation can remove every mapping.
- **Loaders:** `hospes-elf` refuses W+X segments and executable stacks.
- **Shader code generation on the CPU:** `tier2-renderer.md` decided *no JIT in the
  daemon* (AOT in `atrium-spv-compile`, its own process). `GRAPHICS-STACK.md` now says the
  default renderer compiles kernels "in-process by Tier-2 (Cranelift is a default
  feature)", and `atrium-spv-loader/src/jitmap.rs` maps RW, writes, then flips to RX.
  `allow_wx=0` permits that (it is never W and X at once), but it is runtime codegen in a
  long-lived process. **Decide:** either move codegen back out of frescod/the renderer
  (the tier2 decision), or accept it and say which process may do it. Either way, set
  `PROT_MAX(PROT_READ|PROT_WRITE)` on every mapping in processes that must never gain
  execute (FreeBSD's `PROT_MAX`), so RW→RX is impossible there rather than merely unused.

### 5.2 GPU side

The gfx12 PTE has separate READABLE, WRITEABLE and EXECUTABLE bits (`gmc12.c:51-53`).

- **Code is read-only:** `VM_BIND2` with `ATRIUM_GPU_BIND_READONLY` maps without write
  (ABI rev 3); the broker maps F6 app code and constants that way.
- **Data is executable too (gap).** Client BO mappings on silicon always carry EXECUTABLE:
  `si_client.c:1135` uses `READ | EXEC (| WRITE)`, the MES test mappings use RWX
  (`mes12.c:1359`, `:2049`, `:2528`), and GART pages are RWX (`gmc12.c:387`). So a
  shader can write a buffer and then branch into it. The damage is confined to its own
  VM (it cannot reach another VM's pages), so this is not an isolation break, but it lets a
  memory-corruption bug in a shader become code execution in that context.
- **Proposal:** EXECUTABLE only on pages that hold shader code (read-only binds) and
  on PM4 the CP fetches (rings, IBs) if gfx12 requires it there; never on data BOs a
  dispatch can write. Check first which accesses need X on gfx12: the first silicon run
  of 5c-2b faulted the CP's ring read with `PERMISSION_FAULTS 0x8`, which may be the
  execute bit. Add a `test_gpu_isolation` case: a dispatch that writes code into a data
  BO and jumps to it must fault.

## 6. Open items

1. GPU W^X for data BOs (§5.2), and `PROT_MAX` / codegen placement on the CPU (§5.1).
2. Run `test_gpu_isolation` and the F6 hostile-ISA test (G5) on the R9700; then the
   24 h fuzz gate; only then may the stanza say `production`.
3. **User-mode queues and the "broker writes every packet" rule.** A user-mode queue's
   holder writes its own ring. That is fine for frescod's own trusted renderer; it must
   not be handed to an app or a per-client renderer without a decision. Today it is
   root-only. Write the rule into GRAPHICS-STACK and ABI §5.9.
4. **IOMMU.** ISOLATION.md calls it "a second line" on silicon. Confirm whether DMA
   remapping (AMD-Vi / Intel VT-d) is on for the GPU on the R9700 box, and what it covers.
5. **Firmware in the TCB.** PSP, SMU and MES run AMD-signed images we load; MES assigns
   VMIDs to the process contexts we give it (the page-table base stays ours). Note in the
   threat model that a firmware bug is outside what the driver can contain.
6. **RamPages review** (KPI patch 0012): non-contiguous path, `VPO_UNMANAGED` on
   object-less pages against vm_page invariants, a leak counter.
7. **Kernel KASLR is absent** on FreeBSD (kernel-hardening.md §5); an info leak through
   the GPU ioctls would give the kernel layout. The fuzz gate is the defence.
8. **Display path** (gpu-isolation.md open question 1): frescod scans out app-derived
   pixels; the import and blit path must be memory-safe. Display ABI v2 already bounds
   scanout by the BO (break 4).
