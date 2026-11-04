# atrium-gpu-amd: how each isolation invariant is enforced

This maps the invariants in `docs/spec/gpu-isolation.md` (I1–I5) to the
mechanism in this driver, and records how the five breaks from the
2026-11-03 review (`docs/reviews/2026-11-03-gpu-kernel-and-tier2-review.md`
§A3) were closed. The regression is `tests/test_gpu_isolation.c`.

## Invariants

| | Mechanism | Code |
|---|---|---|
| **I1** per-context VA space | Each VM fd owns a VMID and its own page directory and page tables, programmed only by the kernel (`VM_CTX_PT_BASE`). Userspace never writes a PTE. | `vm.c` |
| **I2** per-context BO ownership | Handles are fds. A BO is reachable only through an fd the process holds, and only at the VAs the kernel bound it to in a VM. Another VM's VA misses in that VM's page table and faults. | `bo.c`, `vm.c` |
| **I3** no DMA outside the context's BOs | Every GPU access goes through the submitting queue's VMID. A queue runs one VMID at a time and is never switched while work is in flight (break 1). Scanout is bounded by the BO it was given (break 4). On silicon the IOMMU is a second line. | `queue.c`, `display/display_dev.c` |
| **I4** no privileged commands | Clients submit PM4 rings that the CP runs under their VMID; queue programming, VMIDs, page tables and doorbell routing are kernel-only MMIO. Reset, scheduler and power gating are on the root-only control node (break 5). | `cp.c`, `ioctl.c` |
| **I5** cleanup on teardown | Closing a BO clears its PTEs (with TLB flush) and frees its pages only once no queue that could still use them is busy (break 2). Closing a VM stops every queue under its VMID and points the VMID at an all-invalid page directory before its page tables and VMID are freed. New BOs are zeroed. Closing the device gives back owned queues and revokes their doorbell mappings. | `bo.c`, `vm.c`, `queue.c` |

## The five breaks

1. **Shared-queue reprogramming.** Each hardware queue has a sleepable lock
   around program + doorbell. The queue's read pointer (`QF_RPTR`, gpusim
   1ea93e5) tells the kernel when work is in flight. A submit that would
   switch the queue waits up to 2 s for it to drain, then gets `EBUSY`.
2. **Teardown while in flight.** Deferred-free list for BOs, reaped by a
   timeout task when the queues that were busy at close time have drained or
   been stopped. VM teardown stops its queues first. Counters:
   `sysctl dev.atrium_gpu_amd.0.isolation`.
3. **Doorbell mmap / QUEUE_MAP hijack.** `QUEUE_MAP` makes the calling open
   the queue's exclusive owner (`EBUSY` for anyone else, for maps and
   submits). Doorbells are mmapped only through `d_mmap_single`, only by the
   owner, as a managed device-pager object per grant. The kernel removes the
   page from every mapping when the owner closes the device, the VM goes
   away, or the GPU resets; a later store faults with `SIGBUS`.
4. **Scanout by forgeable offset.** The display ABI v2 commit takes a BO fd,
   not an offset, and the kernel now refuses a buffer whose pitch is shorter
   than a row or whose pitch × height exceeds the BO (`EINVAL`), before the
   referee sees it. Unplugged sinks report no modes.
5. **Unprivileged control.** `GPU_RESET`, `SCHED` and `POWERGATE` return
   `EPERM` on `/dev/atrium-gpu0`. They live on `/dev/atrium-gpu0ctl`, root
   0600, which no jail devfs ruleset unhides.

## Address space

Each VM's GPU-VA window is a `vmem(9)` arena. Binding allocates next-fit, so
a freed range is reused only after a full lap of the window, and a
caller-chosen VA must be page-aligned and clear of every live binding
(`EINVAL` / `EEXIST`, review §A2.5). A range goes back to the arena when its
BO's PTEs are cleared; the pages themselves may still be on the deferred
list. So in the rare case of a lap completing while old work is in flight,
the reused VA can only receive writes from that same VM's own earlier work.
It can never receive another VM's writes.

## Known limits (not isolation breaks, but worth knowing)

- The device still has only two shared queues (gfx and compute). A client
  whose ring parks forever, or faults, holds its queue until its VM closes
  or the control node resets the GPU, so other clients get `EBUSY` on that
  engine meanwhile. That is a denial of service, not a leak. Per-VM MES
  queues (review §A2.3) remove it.
- On gpusim a queue stop and a TLB invalidate take effect at the register
  write. Real silicon needs the HQD dequeue handshake and the invalidate
  acknowledgement before pages are freed. Both are marked in `cp.c` and
  `queue.c`.
- The display node still has no lease: any opener can modeset (review
  §A2.4, display half). That is the Fresco display pipeline's to settle.

## Build identity

`sysctl dev.atrium_gpu_amd.0.build_id` is the last commit that touched this
directory (`build-id.sh`), with `-dirty` for uncommitted changes, and
`dev.atrium_gpu_amd.0.abi_rev` is the client ABI revision (2 since the
isolation fixes). `test_gpu_isolation --attest` prints the
`jaild.policy.toml` stanza for the running build, and
`GpuDrivers::grant_check` in `portcullis/jaild-policy` refuses `gpu` unless
the stanza is `production`, passed, and matches the live build.
