# Plan: USB audio through qemu's xHCI on the Mac

Status: **step A done** (2026-11-25): qemu's xHCI is not the fault on its own (§7). Step B needs
the Sparrow and a sudo session. Goal: a USB DAC passed through to the Mac's
Atrium VM on qemu's xHCI controller plays exactly what Lyra sends, so the hi-res and bit-perfect
path ([lyra-hires.md](lyra-hires.md)) can be tested on real DACs.

## 1. What we know

From 2026-11-13 ([lyra-hires.md](lyra-hires.md); project memory `project_lyra_usb_dac`):

| path | device | result |
|---|---|---|
| guest xHCI → `usb-host` → libusb → macOS | Sparrow (high speed, UAC2) | **buzz** instead of a 440 Hz tone, with 0 underruns in the guest and a clean tone in lyrad's own record |
| guest EHCI → `usb-host` → libusb → macOS | Sparrow | **clean tone** |
| guest UHCI (EHCI companion) → `usb-host` → macOS | DragonFly + PCM2704C (full speed, UAC1) | **the Mac panicked**: a kernel data abort in IOUSBHostFamily, with qemu as the task that triggered it |

The same `usb-host` and libusb code worked under EHCI, so the fault is most likely in the
**xHCI model's isochronous handling**, or in how its pacing meets `usb-host`'s buffering. EHCI
handles only high-speed devices, and most DACs that aren't UAC2 are full speed, so xHCI is the
controller that has to work.

## 2. How the pieces work (read 2026-11-25, QEMU 10.2 in `external/qemu-build`)

**FreeBSD's xHCI driver** (`sys/dev/usb/controller/xhci.c`, `xhci_setup_isoc`) writes one
Isoch TD per (micro)frame: one per microframe for a high-speed endpoint with bInterval 1, one per
1 ms for full speed. Only the first TD after a resync carries a Frame ID (2 ms ahead of
MFINDEX). The rest carry SIA ("as soon as possible").

**qemu's xHCI** (`hw/usb/hcd-xhci.c`) turns each TD into **one** USBPacket (`pkts = 1`) and
paces them on QEMU_CLOCK_VIRTUAL (`xhci_calc_iso_kick`, `kick_timer`):
- A TD with a Frame ID is released at `FrameID << 3`. If that is more than 0x100 microframes
  (32 ms) in the past, it moves **0x4000 microframes (2 s) into the future**.
- An SIA TD is released one interval after the previous one. If the endpoint fell more than four
  intervals behind (half a millisecond at high speed), it is released at the next interval
  boundary instead.
- An empty ring produces a Ring Underrun event.

**qemu's `usb-host`** (`hw/usb/host-libusb.c`) maps **one guest packet to one libusb
isochronous packet**, in a ring of `isobufs` transfers of `isobsize` packets each. The source has
"FIXME: check interval (for now assume one xfer per frame)". For playback (OUT):
- A packet that finds no free buffer is **dropped silently** (`iso_out_of_bufs`, a trace event
  only), and the guest is told it succeeded.
- After the queue runs dry, nothing is sent again until half the buffers have refilled: a gap.
- A packet larger than the endpoint's max packet size is **truncated**.
- For an input endpoint, a packet that arrives with no completed host data returns empty. That
  matters for a UAC2 DAC's **feedback endpoint**, which tells the host its real clock rate.
  uaudio sizes every OUT packet from it.

## 3. Candidate causes

Each one predicts something different that we can measure:

1. **Drops in `usb-host`.** The guest's microframe pacing outruns what libusb drains, or libusb
   on macOS treats a high-speed packet as a 1 ms frame, not a 125 µs microframe. That would play
   7 of every 8 packets late or drop them: a buzz at the frame rate. *Predicts: `out_of_bufs`
   counts and/or start/stop churn.*
2. **Frame ID wrap.** The +0x4000 rule pushes a late resync 2 s ahead, which stalls and then
   bursts. *Predicts: kicks scheduled about 2 s out.*
3. **The feedback endpoint.** Empty feedback packets make uaudio size its OUT packets wrongly,
   so the DAC gets the wrong number of samples per microframe. *Predicts: odd packet sizes in
   the OUT stream, and zero-length IN completions.*
4. **TD splitting.** An isochronous TD that crosses a page boundary spans several TRBs, and they
   may be stitched together wrongly. *Predicts: corrupted bytes in OUT payloads at regular
   offsets.*
5. **Completion events.** A wrong Transfer Event length or ISP handling makes the guest resubmit
   or skip TDs. *Predicts: guest-side resyncs (uaudio debug).*

## 4. Plan

### Phase A: a reproducer with no host USB (no panic risk)

qemu's own emulated `usb-audio` device on the guest's xHCI, writing to a wav file:
`-audiodev wav,id=snd0,path=out.wav -device qemu-xhci,id=xhci -device usb-audio,bus=xhci.0,audiodev=snd0`.
lyrad plays a known signal: a tone plus a frame counter in the low bits, so missing, repeated or
corrupted samples are exact and countable. Compare the wav with `LYRA_DUMP` sample for sample.

- Corrupted here → a pure hcd-xhci bug (causes 2, 4, 5), fixable and testable without real
  hardware, in the cloud VM too.
- Clean here → the fault is in xHCI meeting `usb-host` at high speed (causes 1, 3); go to B.

`usb-audio` is full speed and UAC1, so this covers the full-speed path, the one whose host
passthrough panicked.

### Phase B: measure the real DAC on xHCI (Sparrow only, high speed)

Instrument `usb-host` under a property (`isodebug=on`, off by default):
- counters for packets in, packets submitted, packets dropped (`out_of_bufs`), start/stop
  cycles (underruns), truncated packets, and zero-length feedback reads, printed each second;
- a tap that writes every OUT payload exactly as submitted to libusb to a file.

Then play the same counter signal on the Sparrow, on xHCI and on EHCI, and diff each tap with
`LYRA_DUMP` (S32 = each S16 sample << 16). That says whether it is drops, corruption or timing,
and on which side.

Only the Sparrow: **no full-speed device goes through `usb-host` again** (that panicked the
Mac), and only after you've said a run may go ahead, since it needs sudo.

### Phase C: fix

What Phases A and B show decides this. Likely shapes, by cause:
- **Cause 1:** submit libusb transfers in whole frames for high-speed endpoints (packets per
  transfer a multiple of 8 / interval). Size the ring from the endpoint's interval and the
  measured latency, not a fixed 4 × 32. On a drop, report a missed-service error to the guest,
  not silent success.
- **Cause 2:** compare Frame IDs within the 2048-frame window, and treat a late TD as "now", not
  "two seconds from now".
- **Cause 3:** keep the last real feedback value and return it while the host transfer is in
  flight, not an empty packet.
- **Causes 4 and 5:** fix the TRB walk or the event fields against xHCI 1.2 §4.11.2.3 / §4.14.

Each fix lands with a check that fails before it: the Phase A wav comparison, or a counter that
must stay at 0.

### Phase D: keep it

- `scripts/qemu-xhci-audio-check.sh`: Phase A as a gate (the counter signal must come out with
  0 dropped, 0 repeated and 0 corrupted samples over 60 s at 48 kHz). It runs on the Mac and in
  the cloud VM.
- The fixes go into the atrium-os/qemu fork (QEMU is a development tool here, not shipped, so its
  GPL is fine). They're also worth offering upstream.
- `usb-host-vm.sh` defaults back to xHCI for high-speed devices once B passes. The panic warning
  for full-speed passthrough stays until we understand it (Phase A covers full-speed xHCI
  without touching macOS's USB stack).

## 5. Risks

- **The macOS panic.** That was IOUSBHostFamily's bug, reached through libusb. Phase A needs no
  host USB at all, and Phase B uses only the Sparrow, the device that has never crashed. If
  driving the Sparrow through xHCI's pacing also panics the Mac, we stop host-USB work and finish
  on Phase A plus real hardware.
- **Timing under HVF.** QEMU_CLOCK_VIRTUAL follows the host clock, so main-loop jitter of a
  millisecond or more is normal. The fix must tolerate it with buffering, not depend on punctual
  timers.
- **Scope.** Real DAC hi-res listening still wants real hardware in the end. This makes the Mac
  VM a usable development rig for it.

## 6. Order and size

A (a day) → B (a day, plus one sudo session with you) → C (depends on the cause, 1–3 days) →
D (half a day). A and B are measurement only: nothing in the fork changes behaviour until C.

## 7. As built: Phase A (2026-11-25)

- **The signal**: `LYRA_PLAY_SIGNAL=counter` in lyra-play. Frame n carries n mod 65536 left
  and its complement right, so a recording shows every dropped, repeated or corrupted frame.
  `scripts/lib/audio-counter-check.py` judges a recording; it was checked on a synthetic file
  with 10 dropped, 3 repeated, 1 corrupt and 7 silent frames injected, and counted each one.
- **The rig**: `scripts/qemu-xhci-audio-check.sh`. qemu's `usb-audio` on the guest's xHCI, the
  same device on a UHCI as a control, and the HDA card, each recorded to a wav file on the host.
  `usb-dac-check.sh` plays through whichever lyrad the VM runs. It uses the installed, jailed
  one when present (restarting it, never removing its socket, which the supervisor binds).
- **Two rig faults found first, both qemu's, neither the transport's:**
  - qemu's float mixer is not bit-exact (s16 −3 came out −2). The rig sets `mixing-engine=off`.
  - Without the mixer, qemu's wav writer still paced at the audiodev's 44.1 kHz default. A
    48 kHz device then drained too slowly and dropped about 8% of its 1 ms packets, which looked
    like an xHCI fault. Fixed in the fork: atrium-os/qemu branch `atrium-xhci-isoch`, d1eeb65055
    (`audio/wavaudio.c`: without the mixer, use the voice's own format). The rig needs a qemu
    with it (`QEMU_DIR=`).
- **Result** (Mac VM, HVF, qemu with the fix, 48 kHz S16 counter through the jailed lyrad):

  | path | frames | dropped | repeated | corrupt |
  |---|---|---|---|---|
  | HDA | 1 440 000 | 0 | 0 | 0 |
  | usb-audio on xHCI | 1 440 000, then 2 × 2 880 000 | 0 | 0 | 0 |
  | usb-audio on UHCI | 1 440 000 | 0 | 0 | 0 |

**So xHCI's isochronous scheduling and TD handling carry a full-speed OUT stream exactly.** The
Sparrow's buzz came from the meeting of xHCI and `usb-host`: §3's causes 1 (packet vs microframe
in `usb-host`'s libusb mapping) and 3 (the UAC2 feedback endpoint), or something high-speed
specific. Causes 2, 4 and 5 are unlikely now. Phase B measures which, and needs the Sparrow.
