"""B5 gate. Two Bumble Classic controllers share a LocalLink. The source
controller relays L2CAP SDUs to Rust. Only the sink interprets AVDTP and RTP.
FFmpeg is the independent test decoder; no codec from it ships in Vinculum.
"""
import asyncio
import math
import os
import struct
import subprocess
import sys
from array import array

from bumble import a2dp, avdtp, l2cap
from bumble.controller import Controller
from bumble.core import PhysicalTransport
from bumble.device import Device
from bumble.hci import Address
from bumble.link import LocalLink

MAX_SDU = 672
PACKETS = 48


def device(link, name, addr):
    controller = Controller(name, link=link, public_address=addr)
    dev = Device.with_hci(name, Address(addr), controller, controller)
    dev.classic_enabled = True
    dev.le_enabled = False
    return dev


def check_capture(packets, rate, pool, joint, amplitude_range=(14000, 18500)):
    if len(packets) != PACKETS:
        raise ValueError(f"received {len(packets)} of {PACKETS} media packets")
    seq = packets[0].sequence_number
    timestamp = packets[0].timestamp
    ssrc = packets[0].ssrc
    frames = 0
    encoded = bytearray()
    for packet in packets:
        if (packet.version, packet.payload_type, packet.sequence_number, packet.timestamp, packet.ssrc) != (
            2, 96, seq, timestamp, ssrc
        ):
            raise ValueError("RTP header or progression")
        count = packet.payload[0]
        if not 1 <= count <= 15 or len(bytes(packet)) > MAX_SDU:
            raise ValueError("fragmented or oversized SBC media packet")
        frame_size = 12 + 2 * pool + int(joint)  # 16 blocks, 8 subbands, two channels
        if len(packet.payload) != 1 + count * frame_size:
            raise ValueError("SBC frame count does not fit this RTP payload")
        expected_header = bytes([0x9c, (0xc0 if rate == 48000 else 0x80) | (0x3d if joint else 0x39), pool])
        for pos in range(1, len(packet.payload), frame_size):
            if packet.payload[pos:pos + 3] != expected_header:
                raise ValueError("SBC frame differs from negotiated rate, mode or bitpool")
        encoded.extend(packet.payload[1:])
        seq = (seq + 1) & 0xffff
        timestamp = (timestamp + count * 128) & 0xffffffff
        frames += count * 128

    decoded = subprocess.run(
        [os.environ.get("FFMPEG", "ffmpeg"), "-nostdin", "-v", "error", "-xerror", "-f", "sbc", "-i", "pipe:0",
         "-f", "s16le", "pipe:1"], input=encoded, stdout=subprocess.PIPE,
        check=True, timeout=5
    )
    pcm = array("h")
    pcm.frombytes(decoded.stdout)
    if sys.byteorder != "little":
        pcm.byteswap()
    if len(pcm) != frames * 2:
        raise ValueError(f"decoded {len(pcm)} samples for {frames} stereo frames")
    # Ignore filter start-up and the intentionally padded tail. Project each
    # channel onto its expected sinusoid (arbitrary phase), so swapped
    # channels, silence, noise, or a tone at the wrong rate fail.
    for channel, hz in enumerate([440, 880]):
        samples = pcm[1024 + channel:-1024:2]
        phase = math.tau * hz / rate
        re = sum(x * math.cos(i * phase) for i, x in enumerate(samples))
        im = sum(x * math.sin(i * phase) for i, x in enumerate(samples))
        energy = sum(x * x for x in samples)
        fraction = 2 * (re * re + im * im) / max(1, len(samples) * energy)
        amplitude = 2 * math.hypot(re, im) / len(samples)
        if fraction < 0.95 or not amplitude_range[0] < amplitude < amplitude_range[1]:
            raise ValueError(f"channel {channel}: {hz} Hz fraction {fraction:.3f}, amplitude {amplitude:.0f}")
    print(f"Bumble decoded {frames} stereo frames at {rate} Hz, left 440 Hz and right 880 Hz")


async def run(binary, offer, selected, rate, joint):
    link = LocalLink()
    source = device(link, "B5 source", "F0:F1:F2:F3:F4:01")
    sink = device(link, "B5 sink", "F0:F1:F2:F3:F4:02")
    packets = []
    events = []
    codec = a2dp.SbcMediaCodecInformation.from_bytes(bytes.fromhex(offer))

    @avdtp.Listener.for_device(sink).on("connection")
    def on_signal(protocol):
        endpoint = protocol.add_sink(avdtp.MediaCodecCapabilities(
            avdtp.AVDTP_AUDIO_MEDIA_TYPE, a2dp.A2DP_SBC_CODEC_TYPE, codec))
        def configured():
            caps = [c for c in endpoint.configuration if isinstance(c, avdtp.MediaCodecCapabilities)]
            events.append(bytes(caps[0].media_codec_information))
        endpoint.on("configuration", configured)
        endpoint.on("start", lambda: events.append("start"))
        endpoint.on("close", lambda: events.append("close"))
        endpoint.on("rtp_channel_close", lambda: events.append("released"))
        endpoint.on("rtp_packet", packets.append)

    await source.power_on()
    await sink.power_on()
    await sink.set_connectable(True)
    connection = await source.connect(sink.public_address, transport=PhysicalTransport.BR_EDR)
    spec = l2cap.ClassicChannelSpec(psm=avdtp.AVDTP_PSM, mtu=MAX_SDU)
    signal = await connection.create_l2cap_channel(spec)
    replies = asyncio.Queue(maxsize=1)
    signal.sink = replies.put_nowait
    media = None
    process = await asyncio.create_subprocess_exec(
        binary, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
    try:
        while header := await process.stdout.read(1):
            size = int.from_bytes(await process.stdout.readexactly(2), "big")
            if size > MAX_SDU:
                raise ValueError("oversized SDU from source")
            data = await process.stdout.readexactly(size)
            if header == b"\x01":
                signal.write(data)
                reply = await asyncio.wait_for(replies.get(), 3)
                process.stdin.write(struct.pack(">H", len(reply)) + reply)
                await process.stdin.drain()
            elif header == b"\x02" and not data and media is None:
                media = await connection.create_l2cap_channel(spec)
                process.stdin.write(struct.pack(">H", media.peer_mtu))
                await process.stdin.drain()
            elif header == b"\x03" and media and len(packets) < PACKETS:
                media.write(data)
            elif header == b"\x04" and not data and media:
                await media.disconnect()
                media = None
                process.stdin.write(struct.pack(">H", 0))
                await process.stdin.drain()
            else:
                raise ValueError("unexpected channel operation")
        if await process.wait() != 0:
            raise ValueError("Rust source failed")
        if events != [bytes.fromhex(selected), "start", "close", "released"]:
            raise ValueError(f"AVDTP negotiation/stream lifecycle: {events}")
        check_capture(packets, rate, bytes.fromhex(selected)[-1], joint)
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()
        await connection.disconnect()


async def main(binary):
    await asyncio.wait_for(run(binary, "33150235", "11153333", 48000, True), timeout=15)
    await asyncio.wait_for(run(binary, "22150223", "22152323", 44100, False), timeout=15)


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1]))
