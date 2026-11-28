"""Bumble sink for the jailed B5 gate. The gate signals when both players run."""
import asyncio
from pathlib import Path
import sys

from bumble import a2dp, avdtp
from bumble.device import Device
from bumble.hci import Address
from bumble.keys import MemoryKeyStore
from bumble.pairing import PairingConfig, PairingDelegate
from bumble.transport import open_transport
from peer_a2dp_sink import PACKETS, check_capture


class Consent(PairingDelegate):
    async def compare_numbers(self, number, digits):
        return True


async def run(transport, rate, capture):
    async with await open_transport(transport) as (src, dst):
        dev = Device.with_hci("B5 headphones", Address("F0:F1:F2:F3:F4:02"), src, dst)
        dev.classic_enabled = True
        dev.le_enabled = False
        consent = Consent(io_capability=PairingDelegate.DISPLAY_OUTPUT_AND_YES_NO_INPUT)
        dev.pairing_config_factory = lambda _: PairingConfig(sc=True, mitm=True, bonding=True, delegate=consent)
        dev.sdp_service_records = {0x10001: a2dp.make_audio_sink_service_sdp_records(0x10001)}
        codec, selected = ("11150235", "11153333") if rate == 48000 else ("22150223", "22152323")
        closed = asyncio.Event()
        full = asyncio.Event()
        packets = []
        def packet(p):
            if not full.is_set() and capture.exists():
                packets.append(p)
                if len(packets) == PACKETS:
                    full.set()

        configs = []
        @avdtp.Listener.for_device(dev).on("connection")
        def connected(protocol):
            print("AVDTP connected", flush=True)
            endpoint = protocol.add_sink(avdtp.MediaCodecCapabilities(
                avdtp.AVDTP_AUDIO_MEDIA_TYPE, a2dp.A2DP_SBC_CODEC_TYPE,
                a2dp.SbcMediaCodecInformation.from_bytes(bytes.fromhex(codec))))
            def configured():
                configs.extend(bytes(c.media_codec_information).hex() for c in endpoint.configuration
                               if isinstance(c, avdtp.MediaCodecCapabilities))
            endpoint.on("configuration", configured)
            endpoint.on("start", lambda: print("AVDTP started", flush=True))
            endpoint.on("rtp_packet", packet)
            endpoint.on("rtp_channel_close", closed.set)

        await dev.power_on()
        dev.keystore = MemoryKeyStore()
        await dev.set_connectable(True)
        print("Classic address", dev.public_address.to_string(with_type_qualifier=False), flush=True)
        await full.wait()
        if configs != [selected]:
            raise ValueError(f"unexpected SBC configurations: {configs}")
        check_capture(packets, rate, bytes.fromhex(selected)[-1], rate == 48000, (6000, 9000))
        print("Tone verified", flush=True)
        await closed.wait()
        print("Media channel released", flush=True)


if __name__ == "__main__":
    asyncio.run(asyncio.wait_for(run(sys.argv[1], int(sys.argv[2]), Path(sys.argv[3])), 180))
