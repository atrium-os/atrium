"""A Bumble peer for the Vinculum harness: joins Rootcanal as its own
controller and advertises a name, so the host side (vinculum-probe, later
vinculumd) has a real device to find. Later peers (keyboard, HOGP mouse,
A2DP sink, hostile devices) follow the same shape (docs/spec/bluetooth.md §14).

usage: python peer_advertiser.py HCI_TRANSPORT NAME SECONDS
       e.g. tcp-client:127.0.0.1:6402 VinculumPeer 30
"""
import asyncio
import sys

from bumble.core import AdvertisingData
from bumble.device import Device
from bumble.hci import Address
from bumble.transport import open_transport


async def main(transport: str, name: str, seconds: float) -> None:
    async with await open_transport(transport) as (source, sink):
        device = Device.with_hci(name, Address("F0:F1:F2:F3:F4:F5"), source, sink)
        await device.power_on()
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
    asyncio.run(main(sys.argv[1], sys.argv[2], float(sys.argv[3])))
