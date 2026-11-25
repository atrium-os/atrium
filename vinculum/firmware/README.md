# Controller firmware for the development dongles

Nothing in this directory ships. It says how to get the two development
controllers ready (docs/spec/bluetooth.md §5.2, §15 B1b).

## Raytac MDBT50Q-CX (nRF52840): Zephyr `hci_usb`, flashed once

The dongle runs our own build of Zephyr's `samples/bluetooth/hci_usb`
(Apache-2.0): a standard USB HCI controller (class E0/01/01), Bluetooth LE
only, with Zephyr's open link layer. No loader is needed on the host.

Build (Linux, no west; Zephyr v4.5.0, Ubuntu's arm-none-eabi GCC):

```sh
apt install gcc-arm-none-eabi libnewlib-arm-none-eabi device-tree-compiler ninja-build cmake
pip install pyelftools pyyaml packaging jsonschema
git clone --depth 1 -b v4.5.0 https://github.com/zephyrproject-rtos/zephyr
# modules at the revisions zephyr/west.yml pins:
#   hal_nordic c708b9fe1b7a  -> modules/hal/nordic
#   CMSIS_6    1c1840af7a7e  -> modules/hal/cmsis_6
#   cmsis      512cc7e895e8  -> modules/hal/cmsis
export ZEPHYR_BASE=$PWD/zephyr ZEPHYR_TOOLCHAIN_VARIANT=gnuarmemb GNUARMEMB_TOOLCHAIN_PATH=/usr
cmake -GNinja -S zephyr/samples/bluetooth/hci_usb -B build \
  -DBOARD=raytac_mdbt50q_cx_40_dongle/nrf52840 \
  -DZEPHYR_MODULES="$PWD/modules/hal/nordic;$PWD/modules/hal/cmsis_6;$PWD/modules/hal/cmsis"
ninja -C build        # build/zephyr/zephyr.hex, linked at 0x1000 above the open bootloader
```

The image enumerates as `2fe3:000b` ("Zephyr USBD BT HCI"). The 2026-11-25
build is in the project files as `bluetooth/nrf52840-hci-usb-zephyr-4.5.0.hex`
(sha256 `54fe3999…883c`) and, packaged for the bootloader, `nrf52840-hci-usb-dfu.zip`
(`nrfutil pkg generate --hw-version 52 --sd-req 0x00 --application zephyr.hex
--application-version 1`).

Flash (macOS): hold the dongle's button while plugging it in (the red LED
fades: Nordic's open bootloader), then with Nordic's `nrfutil` and its
`nrf5sdk-tools` command:

```sh
nrfutil nrf5sdk-tools dfu usb-serial -pkg nrf52840-hci-usb-dfu.zip -p /dev/cu.usbmodem*
```

The bootloader is not touched, so this can always be redone or undone.

## TP-Link UB500 (Realtek RTL8761BU): patch at every power-up

`vinculum-hci` loads the patch itself before it relays anything
(`vinculum-hci/src/rtl.rs`, ported from FreeBSD's BSD-2 `rtlbtfw`). It reads
`rtl8761bu_fw.bin` and `rtl8761bu_config.bin` from `--firmware-dir`
(default `/usr/local/share/atrium/firmware/bluetooth`).

The files are Realtek's, from linux-firmware's `rtl_bt/`, under
`LICENSES/LICENCE.rtlwifi_firmware.txt`: redistribution in binary form
without modification is allowed with the copyright notice and disclaimer;
no reverse engineering. So a firmware package may carry them unmodified
with that licence beside them; they are never committed here. The files
checked on 2026-11-25:

| file | bytes | sha256 |
|---|---|---|
| `rtl8761bu_fw.bin` (v1 "Realtech", project 14, 2 patches) | 44484 | `1d7a9597349ad89344fa16c1913d3e39e9a12e966e417ca16871bc79bbe59edb` |
| `rtl8761bu_config.bin` | 6 | `6c28a3f07c6a30ed208c4b64862a23f02b7d93543ea980edd24df16bab45095f` |

`VINCULUM_RTL_FW_DIR=DIR cargo test -p vinculum-hci real_rtl` parses the real
files and loads them into a fake controller.
