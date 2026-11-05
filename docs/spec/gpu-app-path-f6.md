# F6: the app GPU path

*Status 2026-11-05: option A chosen (Shankar). G3's library is built (`atrium-gpu-app`), G2's channel runs on gpusim (`atrium-gpu-fuzz`), and runs an app's own kernel end to end on gpusim (`app-path-e2e`). G1 is done (bb3f7ba4): VM faults fail their work with EIO at once, and app code is bound read-only (ABI rev 3). Threat model §2.5.3 and §2.5.4 row F6. Owner: the GPU driver
thread, with Fresco for the broker wiring.*

## What F6 must end with

An app that brings its own shaders (a 3D app, a game, an ML job, Orbis) runs them on the GPU
with:
- no GPU fd in the app's jail ("apps hold no GPU fds" stays true);
- its own GPU VM, holding only its own buffers and code;
- every PM4 packet written by the broker, never by the app;
- a hang or fault that costs only that app: its work fails with EIO and the others carry on.

## What is already true

- **F1 is done** (`atrium-gpu-amd/ISOLATION.md`). Per-VM page tables, queue ownership, deferred
  frees, revocable doorbells and a root-only control node are in place. The attestation gate
  passes on gpusim (`etc/jaild.policy.toml`), but status stays `experimental` until the
  isolation test has run on silicon.
- **The hang model is done** (622693d1). Completions are tracked per queue. A watchdog stops a
  queue that makes no progress for `hang_ms`, and the work it owed fails with EIO.
- **The split is done** (Fresco F4b to F4d, F3). Each client has a jailed renderer with no GPU
  node, the broker reads only fixed records, and the broker fuzzer exists.
- **The broker already turns validated data into RDNA4 work.** frescod's gpu backend uploads our
  own `bin`/`raster` ISA, builds PM4 with `atrium-rdna4-cmd`, and submits it. The app path
  reuses that machinery, with the app's ISA in the app's VM.

## The trust question (decided: A)

§2.5.3 puts the shader compiler in the app's jail and has the broker receive "machine code plus
the aqueduct-gpu §11 validation record". But anything the app's jail produces, the app controls.
That includes the record. A malicious app can send any machine code with a record that says it
is fine. So for a hostile app, only three things confine it:
1. the GPU MMU: its VMID and the page tables the kernel built (F1);
2. the broker writing every PM4 packet, so app data never reaches the CP as commands;
3. the watchdog, so a hang costs only that app's queue time.

That is also how amdgpu treats user ISA on Linux. Validation (§11's bounded loops and
descriptor coverage) still catches buggy shaders early. But it is not a security boundary for
code an app compiled itself.

Options:
- **A. Untrusted ISA (recommended).** Compile in the app's jail, as §2.5.3 says. The broker
  checks only the code header against hardware limits (VGPR/SGPR counts, LDS size, workgroup
  size, code size) and builds every packet itself. aqueduct-gpu §11 gets amended to say its
  validation is advisory for app-compiled code. This is the smallest piece to build and is
  honest about the boundary.
- **B. A per-app compiler jail run by the broker.** The broker trusts the record from a compiler
  jail it launched. This only helps until a hostile SPIR-V exploits the compiler, and then
  it is the same as A. It also costs an extra process per app.
- **C. Install-time code only.** The broker accepts only code that atrium-pkg's install hook
  compiled and validated, looked up by hash in Tessera, and there is no runtime shader upload.
  This matches §11's wording most closely, but it rules out runtime-generated shaders. It can
  be added later on top of A as a cache.

## Slices (written for option A)

### G1. Driver: what a broker needs to host app VMs (done, bb3f7ba4)
- The broker creates one VM per app on its own device fd. The ABI already supports this.
- Code BOs: today a VM's PTEs are always writable. Add a read-only bind flag so app code and
  constant buffers can't be overwritten by the app's own dispatches. This is hygiene, not
  isolation, since the app can only damage itself.
- Fault reporting: a VM fault should fail that VM's outstanding work with EIO at once, instead
  of waiting `hang_ms` for the watchdog.
- Per-app scheduling weight (deferred): with two shared hardware queues, the `SCHED` weights are per
  queue, not per app. Real per-app weights wait for per-VM MES queues (review §A2.3), which is
  also what removes the brief `EBUSY` one app's work can cause another.

### G2. The broker protocol for app GPU work (built, in review with Fresco)
Each app context is its own `SOCK_SEQPACKET` connection to frescod's GPU socket
(`FRESCOD_GPU_SOCK`), separate from the renderer channel. Reaching the socket is the grant (the
TCB mounts it only for jails with `gpu.access`), and a uid holds at most 4 contexts. Records are
the split's 96-byte framing in their own kind range (`fresco_split::gpu`):
- `HELLO(arch, upload, work)` with two broker-made slots: the broker's GPU architecture id, and
  shared memory for code uploads and work records.
- `CODE(id, bytes)`: a code header and machine code from the upload slot. The broker copies it
  out, checks the header for that architecture, and loads it read-only into the app's VM.
- `BUFFER(id, size)` → `BUFFER_OK(id, va)` with a staging slot; `WRITE` and `READ` copy
  between the staging slot and the buffer (a copy for now; F4e's zero-copy import replaces
  it); `FREE(id)`.
- `SUBMIT(fence, bytes)`: work records from the work slot (bind code, user data, dispatch,
  barrier), built into a ring that is read-only to the GPU, so the app's own dispatches cannot
  rewrite the packets after theirs. Answered `DONE(fence)`, or `LOST(fence, EIO)` after which
  the connection closes and the VM goes with it.
- Anything refused gets `REFUSED(reason, detail)` and the connection closes.

The broker side is `atrium_gpu_app::session`: a `Session` written against an `Arch` trait (code
header checks and the packet builder; `Rdna4` is the one implemented) and a `Backend` trait (the
driver; `AmdBackend` drives atrium-gpu-amd), so another GPU slots in without changing the
channel or the records. Tests: `atrium-gpu-fuzz honest` (8 checks) and `atrium-gpu-fuzz
hostile` against `frescod` with `FRESCOD_GPU_ONLY=1` on gpusim.

Open: a global GPU memory budget across contexts (today one app can use up to 256 MiB, and
gpusim answers ENOSPC once the pool is full), and folding the hostile run into
`scripts/fresco-split-fuzz.sh`.

### G3. Broker PM4 builder (library done)
- `atrium-gpu-app` (6dcd221f): code header checks, the closed record set (bind code, user
  SGPRs, dispatch, barrier), and a builder that writes every packet with `atrium-rdna4-cmd`. A
  mutation fuzzer is built in, and it found an overflow on its first run. `app-path-e2e` (feature
  `e2e`) stands in for the broker on the device: an app's own kernel runs in its own VM, the
  same kernel aimed at that VA from a second app's VM writes only that app's page, and bad
  headers and oversized dispatches are refused before anything is submitted.
- A library crate turns a validated frame into PM4 with `atrium-rdna4-cmd`, using the app's VAs
  and the broker's own syncobj.
- On EIO the app's GPU context is lost: the broker reports it, drops the app's VM, and the app
  starts again with a fresh one.

### G4. Tests on gpusim (hostile apps) (passing: `atrium-gpu-fuzz isa` and `hostile`)
- An honest app's compute shader fills a buffer that it then reads back.
- Hostile ISA:
  - an infinite loop: the watchdog fails it with EIO, and another app's dispatch runs within
    `hang_ms`;
  - loads and stores at another app's VAs: they fault in the hostile app's VM, and nothing leaks;
  - huge dims and bad headers: the broker refuses them.
- The broker records are fuzzed (an F3 extension).

Status 2026-11-05 on gpusim 354fffb: all of the above pass through frescod (`atrium-gpu-fuzz isa`:
a self-branching kernel is lost with EIO while another app's dispatch completes in 5 s; stores
at VA 0, a high VA and past the app's buffer are each lost with EIO; an honest app is served
afterwards). The spinning kernel took about 20 s to be failed instead of `hang_ms` (5 s),
because gpusim runs a dispatch inside the doorbell write, so the guest vCPU is held until the
simulator's own per-dispatch budget runs out. Real hardware queues are asynchronous, so G5 has
to measure the real time to fail.

### G5. On silicon (needs Shankar's OK)
`test_gpu_isolation` and the hostile-ISA test on the R9700. They deliberately cause VM faults
and hangs on a shared GPU, so they run only when it is quiet and Shankar says so. Passing them
is what lets the stanza go to `production`.

## Order
G1 (driver, done 37148ed4), then G3 (library, testable without the broker), then G2 (with Fresco), then G4,
then G5.
