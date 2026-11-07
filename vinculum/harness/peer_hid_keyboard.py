"""A Bumble Classic HID keyboard for the Vinculum harness (B3, spec §14).

usage: python peer_hid_keyboard.py HCI_TRANSPORT SECONDS HOST_ADDR

Joins Rootcanal as its own controller with page scan on, an SDP HID record
(boot keyboard descriptor, no report ids) and a PnP record, and pairs with
numeric comparison (display-yes-no: an authenticated key, so keyboard
reports are allowed, §7).

1. vinculumd pages it, pairs, reads SDP and opens control and interrupt:
   once both are open the keyboard types "ab" (press, release each).
2. It then drops the link, waits, and pages HOST_ADDR itself, the way a
   bonded keyboard reconnects when a key is pressed. Once the link is
   encrypted it opens control and interrupt itself and types "c".
"""
import asyncio
import sys

from bumble import hid, sdp
from bumble.core import UUID, PhysicalTransport
from bumble.device import Device
from bumble.hci import Address
from bumble.keys import MemoryKeyStore
from bumble.pairing import PairingConfig, PairingDelegate
from bumble.transport import open_transport

# Boot keyboard: modifiers, reserved, six keys (no report ids).
DESCRIPTOR = bytes(
    [
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,
        0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
        0xC0,
    ]
)

KEY = {"a": 0x04, "b": 0x05, "c": 0x06}

DE = sdp.DataElement
SA = sdp.ServiceAttribute


def records():
    hid_rec = [
        SA(sdp.SDP_SERVICE_RECORD_HANDLE_ATTRIBUTE_ID, DE.unsigned_integer_32(0x00010001)),
        SA(sdp.SDP_SERVICE_CLASS_ID_LIST_ATTRIBUTE_ID, DE.sequence([DE.uuid(UUID.from_16_bits(0x1124))])),
        SA(
            sdp.SDP_PROTOCOL_DESCRIPTOR_LIST_ATTRIBUTE_ID,
            DE.sequence(
                [
                    DE.sequence([DE.uuid(UUID.from_16_bits(0x0100)), DE.unsigned_integer_16(0x0011)]),
                    DE.sequence([DE.uuid(UUID.from_16_bits(0x0011))]),
                ]
            ),
        ),
        SA(0x0100, DE.text_string(b"Vinculum Test Keyboard")),
        SA(0x0206, DE.sequence([DE.sequence([DE.unsigned_integer_8(0x22), DE.text_string(DESCRIPTOR)])])),
    ]
    pnp = [
        SA(sdp.SDP_SERVICE_RECORD_HANDLE_ATTRIBUTE_ID, DE.unsigned_integer_32(0x00010002)),
        SA(sdp.SDP_SERVICE_CLASS_ID_LIST_ATTRIBUTE_ID, DE.sequence([DE.uuid(UUID.from_16_bits(0x1200))])),
        SA(0x0201, DE.unsigned_integer_16(0x1234)),
        SA(0x0202, DE.unsigned_integer_16(0x5678)),
        SA(0x0203, DE.unsigned_integer_16(0x0100)),
    ]
    return {0x00010001: hid_rec, 0x00010002: pnp}


class Delegate(PairingDelegate):
    def __init__(self):
        super().__init__(io_capability=PairingDelegate.DISPLAY_OUTPUT_AND_YES_NO_INPUT)

    async def compare_numbers(self, number, digits):
        print(f"peer compares {number:06d}", flush=True)
        return True


def report(key):
    return bytes([0, 0, key, 0, 0, 0, 0, 0])


async def type_keys(kbd, text):
    for ch in text:
        kbd.send_data(report(KEY[ch]))
        await asyncio.sleep(0.05)
        kbd.send_data(report(0))
        await asyncio.sleep(0.05)
    print(f"peer typed {text!r}", flush=True)


async def main(transport: str, seconds: float, host_addr: str) -> None:
    async with await open_transport(transport) as (source, sink):
        device = Device.with_hci("VinculumKbd", Address("F0:F1:F2:F3:F4:F6"), source, sink)
        delegate = Delegate()
        device.pairing_config_factory = lambda _c: PairingConfig(sc=True, mitm=True, bonding=True, delegate=delegate)
        device.classic_enabled = True
        device.le_enabled = False
        device.sdp_service_records = records()
        kbd = hid.Device(device)
        await device.power_on()
        device.keystore = MemoryKeyStore()
        await device.set_connectable(True)
        print(f"classic address {device.public_address.to_string(with_type_qualifier=False)}", flush=True)

        both_open = asyncio.Event()
        encrypted = asyncio.Event()

        def check_open(*_):
            if kbd.l2cap_ctrl_channel is not None and kbd.l2cap_intr_channel is not None:
                both_open.set()

        # hid.Device learns of channels the host opens through these.
        orig = kbd.on_l2cap_channel_open

        def on_open(ch):
            orig(ch)
            print(f"peer HID channel {ch.psm:#x} open", flush=True)
            check_open()

        kbd.on_l2cap_channel_open = on_open
        kbd.on("interrupt_data", lambda pdu: print(f"peer output report {pdu.hex()}", flush=True))

        @device.on("connection")
        def on_connection(conn):
            print(f"peer connected {conn.peer_address}", flush=True)
            conn.on("pairing", lambda keys: print("peer paired", flush=True))
            conn.on("pairing_failure", lambda reason: print(f"peer pairing failed: {reason}", flush=True))

            def on_enc(*_):
                print(f"peer encryption {conn.encryption}", flush=True)
                if conn.encryption:
                    encrypted.set()

            conn.on("connection_encryption_change", on_enc)

        # 1: the host connects, pairs and opens the channels.
        await asyncio.wait_for(both_open.wait(), seconds)
        await asyncio.sleep(0.3)
        await type_keys(kbd, "ab")
        await asyncio.sleep(1.0)

        # 2: drop the link and reconnect from our side.
        conn = kbd.connection
        await conn.disconnect()
        print("peer dropped the link", flush=True)
        await asyncio.sleep(2.0)
        both_open.clear()
        encrypted.clear()
        conn = await device.connect(host_addr, transport=PhysicalTransport.BR_EDR)
        print("peer reconnected", flush=True)
        await asyncio.wait_for(encrypted.wait(), 10)
        # The host may open them itself after SDP; whichever comes first.
        await asyncio.sleep(0.2)
        try:
            if kbd.l2cap_ctrl_channel is None:
                await kbd.connect_control_channel()
            if kbd.l2cap_intr_channel is None:
                await kbd.connect_interrupt_channel()
        except Exception as e:  # noqa: BLE001 - the host's own channels may win
            print(f"peer channel open: {e}", flush=True)
        check_open()
        await asyncio.wait_for(both_open.wait(), 10)
        await asyncio.sleep(1.0)
        await type_keys(kbd, "c")
        await asyncio.sleep(seconds)


if __name__ == "__main__":
    a = sys.argv
    asyncio.run(main(a[1], float(a[2]), a[3]))
