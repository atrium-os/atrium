"""A Bumble peer for the Vinculum harness: joins Rootcanal as its own
controller and advertises a name, so the host side (vinculum-probe, later
vinculumd) has a real device to find. Later peers (keyboard, HOGP mouse,
A2DP sink, hostile devices) follow the same shape (docs/spec/bluetooth.md §14).

usage: python peer_advertiser.py HCI_TRANSPORT NAME SECONDS [IOCAP [PASSKEY_FILE]]
       e.g. tcp-client:127.0.0.1:6402 VinculumPeer 30 display-yes-no

IOCAP is the peer's pairing I/O capability (none, display, display-yes-no,
keyboard, keyboard-display); the peer accepts every pairing (B2, §15). A
keyboard peer types the passkey the host displays, read from PASSKEY_FILE.
"""
import asyncio
import sys

from bumble.core import AdvertisingData
from bumble.device import Device
from bumble.hci import Address
from bumble.keys import MemoryKeyStore
from bumble.pairing import PairingConfig, PairingDelegate
from bumble.transport import open_transport


IOCAPS = {
    "none": PairingDelegate.NO_OUTPUT_NO_INPUT,
    "display": PairingDelegate.DISPLAY_OUTPUT_ONLY,
    "display-yes-no": PairingDelegate.DISPLAY_OUTPUT_AND_YES_NO_INPUT,
    "keyboard": PairingDelegate.KEYBOARD_INPUT_ONLY,
    "keyboard-display": PairingDelegate.DISPLAY_OUTPUT_AND_KEYBOARD_INPUT,
}


class Delegate(PairingDelegate):
    def __init__(self, io, passkey_file):
        super().__init__(io_capability=io)
        self.passkey_file = passkey_file

    async def compare_numbers(self, number, digits):
        print(f"peer compares {number:06d}", flush=True)
        return True

    async def display_number(self, number, digits):
        print(f"peer displays {number:06d}", flush=True)

    async def get_number(self):
        for _ in range(100):
            try:
                with open(self.passkey_file) as f:
                    v = f.read().strip()
                if v:
                    print(f"peer types {v}", flush=True)
                    return int(v)
            except (OSError, TypeError):
                pass
            await asyncio.sleep(0.1)
        return None


async def main(transport: str, name: str, seconds: float, iocap: str, passkey_file) -> None:
    async with await open_transport(transport) as (source, sink):
        device = Device.with_hci(name, Address("F0:F1:F2:F3:F4:F5"), source, sink)
        delegate = Delegate(IOCAPS[iocap], passkey_file)
        device.pairing_config_factory = lambda _c: PairingConfig(sc=True, mitm=True, bonding=True, delegate=delegate)
        # Classic too (page scan, SSP with Secure Connections), so the
        # BR/EDR scenarios can connect to the same peer by address.
        device.classic_enabled = True
        await device.power_on()
        device.keystore = MemoryKeyStore()
        await device.set_connectable(True)
        print(f"classic address {device.public_address.to_string(with_type_qualifier=False)}", flush=True)

        @device.on("connection")
        def on_connection(conn):
            print(f"peer connected {conn.peer_address}", flush=True)
            conn.on("pairing", lambda keys: print(f"peer paired: ltk {'yes' if keys.ltk else 'no'}", flush=True))
            conn.on("pairing_failure", lambda reason: print(f"peer pairing failed: {reason}", flush=True))
            conn.on("connection_encryption_change", lambda *a: print(f"peer encryption {conn.encryption}", flush=True))

        device.advertising_data = bytes(
            AdvertisingData(
                [
                    (AdvertisingData.FLAGS, bytes([0x06])),
                    (AdvertisingData.COMPLETE_LOCAL_NAME, name.encode()),
                ]
            )
        )
        await device.start_advertising(auto_restart=True)
        print(f"advertising {name!r}", flush=True)
        await asyncio.sleep(seconds)


if __name__ == "__main__":
    a = sys.argv
    asyncio.run(main(a[1], a[2], float(a[3]), a[4] if len(a) > 4 else "none", a[5] if len(a) > 5 else None))
