# Local changes to the vendored PDL grammars

`hci_packets.pdl` is Android's grammar (see NOTICE). Vinculum's additions:

- 2026-11-25: `ZEPHYR_READ_STATIC_ADDRESSES = 0xFC09` in `OpCode`, and the
  `ZephyrStaticAddress` struct and `ZephyrReadStaticAddresses(Complete)`
  packets at the end of the file. Zephyr's `hci_usb` controllers (the
  nRF52840 development dongle) report BD_ADDR 00:00:00:00:00:00 and give
  their factory-random static address through this read-only vendor command
  (Zephyr `BT_HCI_OP_VS_READ_STATIC_ADDRS`).
