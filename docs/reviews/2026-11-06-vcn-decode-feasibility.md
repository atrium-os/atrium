# Hardware video decode (VCN) feasibility

**Date:** 2026-11-06. **Decision (Shankar, 2026-11-06): A, park G1 and G2** until atrium-gpu-amd runs on the R9700 with PSP loading; decode stays on the CPU. Revisit the source rule (C suggested) then. **Covers:** items G1 and G2 in
[fresco-video.md](../spec/fresco-video.md) §8 and decision §9.4.

## Short answer

Hardware decode on the R9700 is legally possible, but it can't be built clean-room the way
gpusim's shader core was. There is no AMD prose document for the VCN block. What exists is
AMD-authored MIT code: register headers, the decode message structures, and the init
sequence inside the Linux and Mesa drivers. It is also blocked on silicon bring-up that
hasn't started, because the VCN firmware is loaded by the PSP and atrium-gpu-amd has no PSP
code yet. My recommendation is to keep decode on the CPU for now and pick G1 up once the
driver runs on the R9700 (x86_64 port phase D).

## 1. What public material exists

| Need | Public source | Kind |
|---|---|---|
| VCN 5.0 register offsets and fields | Linux `asic_reg/vcn/vcn_5_0_0_offset.h`, `_sh_mask.h` | AMD-authored MIT headers, the same kind as the `gc_12_0_0` headers already in `atrium-rdna4-cmd/spec/` |
| Decode message format (per-codec picture params, buffers, DPB) | Mesa `src/amd/common/ac_vcn_dec.h` and the unified-queue IB headers | AMD-authored MIT headers, structures only |
| Ring/queue setup, DPG power gating, firmware shared memory, start/stop order | Linux `vcn_v5_0_0.c`, `amdgpu_vcn.c` | MIT driver **code**; no prose description anywhere |
| Firmware load | the PSP front-door load (`psp_v14_0`), signed blob `vcn_5_0_0.bin` | MIT driver code; the blob itself is binary-only, no reverse engineering |
| App-level programming guide | AMD AMF decoder guide (gpuopen) | sits on top of a full driver; says nothing about the hardware |

A web search found no register-level VCN reference from AMD
([Wikipedia overview](https://www.wikiwand.com/en/articles/Video_Core_Next),
[Emby's VCN notes](https://emby.media/learn/articles/hardware-acceleration/interfaces/amd/Hardware-AMD-VCN.html)
point only at AMF and the Linux driver).

So the headers can be used under our current rule, since they are interface definitions like
the GFX12 headers. The bring-up order can't: it exists only as driver logic. Learning it by
black-box probing on the card is slow, and probing the firmware's behaviour comes close to
the "no reverse engineering" term in the firmware licence
([amdgpu-firmware licence](/mnt/project-files/graphics/rdna4/LICENSE.amdgpu.txt)).

## 2. What G1 would take

1. **PSP bring-up on silicon.** Every RDNA4 firmware (CP, MES, SDMA, VCN) is loaded through
   the PSP. Today `firmware.c` only models that step for gpusim (`regFW_CP_LOAD`). Real PSP
   support is needed for GFX on silicon anyway, so it belongs to phase D, not to video.
2. **VCN block in atrium-gpu-amd:** power and clock ungating through the SMU, firmware shared
   memory, the unified decode queue, interrupts, reset, and putting each session under the
   client's VMID so the F6 boundary (page tables plus watchdog) holds.
3. **Player side:** the player still parses the bitstream headers (SPS/PPS, AV1 sequence and
   frame headers) in its jail and fills the message structures, as VA-API drivers do. VCN does
   the entropy decode and reconstruction.
4. **gpusim model:** a VCN device that accepts the same messages and decodes with a software
   decoder (dav1d or similar) inside gpusim. Its accuracy can only be checked against silicon.

Rough size: 1 is large and shared with phase D; 2 and 4 are each comparable to the display
module's D1 to D4; 3 is per codec (AV1, HEVC, H.264, VP9).

## 3. Security

Moving decode to VCN moves the entropy decoder from our jailed CPU process into closed
firmware that has DMA. The VMID page tables still bound what it can touch, so a firmware bug
reaches only the player's own buffers, provided every session runs under the client's VMID.
That has to be tested on silicon (`test_gpu_isolation`) before it ships. CPU decode in the
player's jail avoids this whole question.

## 4. Is CPU decode enough?

For now, yes. dav1d plays 4K60 AV1 on a few cores of a desktop CPU like the R9700 box's
E-2288G, and on the Mac the arm64 VM has no AMD GPU at all. Hardware decode mostly buys power
and frees the CPU, which matters on laptops and for 8K. The video layer API in fresco-video.md
doesn't change when hardware frames arrive (§7, §9.2), so waiting costs no rework.

## 5. G2 (GPU frames into video layers)

G2 only helps when frames are born in VRAM, which means after G1. The ABI side is already
there: sealed BOs (`ATRIUM_GPU_BOQ_SEALED`) and read-only binds (`ATRIUM_GPU_BIND_READ_ONLY`)
in `gpu_abi_v2.h` §5.13. It also needs F6 at production status. I'd park G2 with G1.

## 6. Options

- **A. Park G1 and G2 (recommended).** CPU decode in the player's jail. Revisit when
  atrium-gpu-amd runs on the R9700 with PSP loading, and decide the source rule then.
- **B. Start now, headers only.** Build the gpusim VCN model and the driver's queue path from
  the MIT headers alone, and work out the init order on silicon later. Risk: gpusim becomes
  our guess at VCN, with nothing to check it against for months.
- **C. Start now, and allow reading AMD's MIT VCN driver code** (Linux `vcn_v5_0_0.c`, Mesa's
  decoder) for VCN only, with attribution. Licence-wise this is fine, since it's MIT. It ends
  the clean-room stance for this one block, and the code we write would follow AMD's
  sequences. It is the fastest and most reliable path when we do build it.

If A is chosen, I'd suggest C as the source rule when G1 resumes, because headers alone
don't give a bring-up order.
