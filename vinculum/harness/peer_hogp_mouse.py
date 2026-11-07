"""A Bumble HID-over-GATT mouse for the Vinculum harness (B4, spec §14).

usage: python peer_hogp_mouse.py HCI_TRANSPORT SECONDS

Advertises "VinculumMouse" over LE with a GATT database: HID service
(Report Map with report id 1, one input report with its Report Reference,
HID Information, Control Point), Device Information (PnP ID 1234:5678) and
Battery (77 %, notifying). Pairs Just Works or numeric comparison, as the
host asks (a mouse needs no keyboard rights). Characteristic values need
an encrypted link.

Each time the host subscribes to the input report it moves right 3 times
by 10 and clicks the left button; the first time it also reports the
battery at 60 % and then drops the link, so the host can reconnect.
"""
import asyncio
import sys

from bumble import gatt
from bumble.core import AdvertisingData
from bumble.device import Device
from bumble.hci import Address
from bumble.keys import MemoryKeyStore
from bumble.pairing import PairingConfig, PairingDelegate
from bumble.transport import open_transport

# Mouse with report id 1: 3 buttons, 5 bits padding, X, Y (relative, -127..127).
REPORT_MAP = bytes(
    [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
        0xC0, 0xC0,
    ]
)

ENC = gatt.Characteristic.READ_REQUIRES_ENCRYPTION
P = gatt.Characteristic.Properties


class Delegate(PairingDelegate):
    def __init__(self):
        super().__init__(io_capability=PairingDelegate.DISPLAY_OUTPUT_AND_YES_NO_INPUT)

    async def compare_numbers(self, number, digits):
        print(f"peer compares {number:06d}", flush=True)
        return True


async def main(transport: str, seconds: float) -> None:
    async with await open_transport(transport) as (source, sink):
        device = Device.with_hci("VinculumMouse", Address("F0:F1:F2:F3:F4:F7"), source, sink)
        delegate = Delegate()
        device.pairing_config_factory = lambda _c: PairingConfig(sc=True, mitm=True, bonding=True, delegate=delegate)

        report = gatt.Characteristic(
            "2A4D", P.READ | P.NOTIFY, ENC, bytes(3),
            descriptors=[gatt.Descriptor(gatt.GATT_REPORT_REFERENCE_DESCRIPTOR, ENC, bytes([1, 1]))],
        )
        battery = gatt.Characteristic("2A19", P.READ | P.NOTIFY, ENC, bytes([77]))
        device.add_services(
            [
                gatt.Service(
                    "1812",
                    [
                        gatt.Characteristic("2A4B", P.READ, ENC, REPORT_MAP),
                        report,
                        gatt.Characteristic("2A4A", P.READ, ENC, bytes([0x11, 0x01, 0x00, 0x02])),
                        gatt.Characteristic("2A4C", P.WRITE_WITHOUT_RESPONSE, gatt.Characteristic.WRITEABLE, bytes(1)),
                    ],
                ),
                gatt.Service("180A", [gatt.Characteristic("2A50", P.READ, ENC, bytes([2, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01]))]),
                gatt.Service("180F", [battery]),
            ]
        )
        await device.power_on()
        device.keystore = MemoryKeyStore()
        rounds = {"n": 0}

        async def move(conn):
            await asyncio.sleep(0.5)
            for _ in range(3):
                await device.notify_subscriber(conn, report, bytes([0, 10, 0]))
                await asyncio.sleep(0.05)
            await device.notify_subscriber(conn, report, bytes([1, 0, 0]))
            await asyncio.sleep(0.05)
            await device.notify_subscriber(conn, report, bytes([0, 0, 0]))
            print("peer moved and clicked", flush=True)
            rounds["n"] += 1
            if rounds["n"] == 1:
                battery.value = bytes([60])
                await device.notify_subscriber(conn, battery, bytes([60]))
                await asyncio.sleep(1.0)
                await conn.disconnect()
                print("peer dropped the link", flush=True)

        def on_subscription(conn, notify, indicate):
            print(f"peer input report subscription notify={notify}", flush=True)
            if notify:
                asyncio.ensure_future(move(conn))

        report.on("subscription", on_subscription)

        @device.on("connection")
        def on_connection(conn):
            print(f"peer connected {conn.peer_address}", flush=True)
            conn.on("pairing", lambda keys: print("peer paired", flush=True))

        device.advertising_data = bytes(
            AdvertisingData(
                [
                    (AdvertisingData.FLAGS, bytes([0x06])),
                    (AdvertisingData.COMPLETE_LOCAL_NAME, b"VinculumMouse"),
                    (AdvertisingData.APPEARANCE, (0x03C2).to_bytes(2, "little")),
                    (AdvertisingData.INCOMPLETE_LIST_OF_16_BIT_SERVICE_CLASS_UUIDS, (0x1812).to_bytes(2, "little")),
                ]
            )
        )
        await device.start_advertising(auto_restart=True)
        print("advertising 'VinculumMouse'", flush=True)
        await asyncio.sleep(seconds)


if __name__ == "__main__":
    a = sys.argv
    asyncio.run(main(a[1], float(a[2])))
