//! Initiator side of one AVDTP signalling session and its A2DP/SBC media stream.
//! The channel owner must close both channels on any error or link loss.
#![forbid(unsafe_code)]
#[allow(warnings, clippy::all)]
mod avdtp {
    include!(concat!(env!("OUT_DIR"), "/avdtp.rs"));
}
use nanoaudio_core::codec::Encoder;
use nanoaudio_sbc::{SbcChannelMode, SbcEncoder, SbcParameters};
use pdl_runtime::Packet;
use rand_core::{OsRng, RngCore};
use std::io::{self, ErrorKind};

/// Vinculum's basic-mode L2CAP SDU limit.
pub const MAX_SDU: usize = 672;
const DISCOVER: u8 = 1;
const GET_CAPABILITIES: u8 = 2;
const SET_CONFIGURATION: u8 = 3;
const OPEN: u8 = 6;
const START: u8 = 7;
const CLOSE: u8 = 8;
const SBC_BLOCKS_BANDS_LOUDNESS: u8 = 0x15;
const HEADER: usize = 13; // RTP plus the SBC payload header.
const LOCAL_SEID: u8 = 1 << 2;
const FRAMES_PER_SBC: usize = 128; // 16 blocks of 8 subbands.
const MAX_SBC_PER_PACKET: usize = 5;

/// Already-authorized AVDTP channels on a single Classic link. An exchange
/// has a deadline, returns one complete SDU and rejects oversized SDUs.
pub trait Channels {
    fn exchange(&mut self, command: &[u8], reply: &mut [u8; MAX_SDU]) -> io::Result<usize>;
    /// Open the second channel on the same PSM and return its peer MTU.
    fn open_media(&mut self) -> io::Result<u16>;
    fn send_media(&mut self, packet: &[u8]) -> io::Result<()>;
    fn close_media(&mut self) -> io::Result<()>;
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, reason)
}

struct Signalling {
    label: u8,
}

impl Signalling {
    fn command(
        &mut self,
        channels: &mut impl Channels,
        signal: u8,
        payload: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut reply = [0; MAX_SDU];
        let label = self.label;
        self.label = (self.label + 1) & 15;
        let mut packet = vec![label << 4, signal];
        packet.extend_from_slice(payload);
        let len = channels.exchange(&packet, &mut reply)?;
        let bytes = reply
            .get(..len)
            .ok_or_else(|| invalid("oversized AVDTP reply"))?;
        let result =
            avdtp::Signal::decode_full(bytes).map_err(|_| invalid("malformed AVDTP reply"))?;
        if result.label != label
            || result.packet_type != 0
            || result.message_type != 2
            || result.signal_id != signal
        {
            return Err(invalid("unexpected, fragmented or rejected AVDTP response"));
        }
        Ok(result.body)
    }

    fn accept(
        &mut self,
        channels: &mut impl Channels,
        signal: u8,
        payload: &[u8],
    ) -> io::Result<()> {
        if self.command(channels, signal, payload)?.is_empty() {
            Ok(())
        } else {
            Err(invalid("unexpected payload in AVDTP acceptance"))
        }
    }
}

#[derive(Clone, Copy)]
struct Config {
    frequency_mode: u8,
    parameters: SbcParameters,
}

impl Config {
    fn from_capabilities(data: &[u8]) -> io::Result<Option<Self>> {
        let capabilities = avdtp::Capabilities::decode_full(data)
            .map_err(|_| invalid("malformed AVDTP capabilities"))?;
        let mut transport = false;
        let mut codec = None;
        for cap in capabilities.list {
            match cap.category {
                1 if transport || !cap.value.is_empty() => {
                    return Err(invalid("invalid media transport capability"))
                }
                1 => transport = true,
                7 if codec.is_some() => return Err(invalid("duplicate media codec capability")),
                7 => codec = Some(cap.value),
                _ => {}
            }
        }
        let Some(codec) = codec else { return Ok(None) };
        let Ok(avdtp::SbcCodec {
            media_type: 0,
            codec_type: 0,
            modes,
            blocks,
            minimum: min,
            maximum: max,
        }) = avdtp::SbcCodec::decode_full(&codec)
        else {
            return Ok(None);
        };
        // Audio SBC, 16 blocks, 8 subbands, loudness allocation.
        if !transport
            || blocks & SBC_BLOCKS_BANDS_LOUDNESS != SBC_BLOCKS_BANDS_LOUDNESS
            || min < 2
            || min > max
            || max > 250
        {
            return Ok(None);
        }
        let (frequency, rate, target) = if modes & 0x10 != 0 {
            (0x10, 48_000, 51)
        } else if modes & 0x20 != 0 {
            (0x20, 44_100, 53)
        } else {
            return Ok(None);
        };
        let (mode, channel_mode) = if modes & 1 != 0 {
            (1, SbcChannelMode::JointStereo)
        } else if modes & 2 != 0 {
            (2, SbcChannelMode::Stereo)
        } else {
            return Ok(None);
        };
        let bitpool = target.min(max);
        if bitpool < min {
            return Ok(None);
        }
        Ok(Some(Self {
            frequency_mode: frequency | mode,
            parameters: SbcParameters {
                sample_rate: rate,
                channel_mode,
                bitpool,
                ..SbcParameters::HIGH_QUALITY
            },
        }))
    }
}

/// A started stream. The encoder keeps its analysis filter across packets.
pub struct Stream<C> {
    channels: C,
    signalling: Signalling,
    seid: u8,
    encoder: SbcEncoder,
    sbc_frames: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
}

impl<C: Channels> Stream<C> {
    pub fn start(mut channels: C) -> io::Result<Self> {
        let mut signalling = Signalling { label: 0 };
        let discovery = signalling.command(&mut channels, DISCOVER, &[])?;
        if discovery.len() > 62 * 2 {
            return Err(invalid("oversized AVDTP endpoint list"));
        }
        let endpoints = avdtp::Discovery::decode_full(&discovery)
            .map_err(|_| invalid("malformed AVDTP endpoint list"))?;
        for endpoint in endpoints.endpoints {
            if !(1..=62).contains(&endpoint.seid)
                || endpoint.in_use != 0
                || endpoint.sink != 1
                || endpoint.media_type != 0
            {
                continue;
            }
            let seid = endpoint.seid << 2;
            let capabilities = signalling.command(&mut channels, GET_CAPABILITIES, &[seid])?;
            let Some(config) = Config::from_capabilities(&capabilities)? else {
                continue;
            };
            let pool = config.parameters.bitpool;
            signalling.accept(
                &mut channels,
                SET_CONFIGURATION,
                &[
                    seid,
                    LOCAL_SEID,
                    1,
                    0,
                    7,
                    6,
                    0,
                    0,
                    config.frequency_mode,
                    SBC_BLOCKS_BANDS_LOUDNESS,
                    pool,
                    pool,
                ],
            )?;
            signalling.accept(&mut channels, OPEN, &[seid])?;
            let mtu = usize::from(channels.open_media()?).min(MAX_SDU);
            let count = (mtu.saturating_sub(HEADER) / config.parameters.frame_length())
                .min(MAX_SBC_PER_PACKET);
            if count == 0 {
                return Err(invalid("media MTU cannot hold one SBC frame"));
            }
            let encoder = SbcEncoder::new(config.parameters)
                .map_err(|_| invalid("invalid SBC configuration"))?;
            let mut seed = [0; 10];
            OsRng
                .try_fill_bytes(&mut seed)
                .map_err(|err| io::Error::other(err.to_string()))?;
            signalling.accept(&mut channels, START, &[seid])?;
            let [a, b, c, d, e, f, g, h, i, j] = seed;
            return Ok(Self {
                channels,
                signalling,
                seid,
                encoder,
                sbc_frames: u8::try_from(count).map_err(|_| invalid("SBC count"))?,
                sequence: u16::from_be_bytes([a, b]),
                timestamp: u32::from_be_bytes([c, d, e, f]),
                ssrc: u32::from_be_bytes([g, h, i, j]),
            });
        }
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "no available stereo SBC sink",
        ))
    }

    pub fn rate(&self) -> u32 {
        self.encoder.parameters().sample_rate
    }
    pub fn pcm_frames(&self) -> usize {
        usize::from(self.sbc_frames) * FRAMES_PER_SBC
    }

    pub fn send(&mut self, stereo_pcm: &[f32]) -> io::Result<()> {
        if stereo_pcm.len() != self.pcm_frames() * 2 {
            return Err(invalid("incorrect stereo PCM packet length"));
        }
        let mut packet = [0; MAX_SDU];
        packet[0..2].copy_from_slice(&[0x80, 96]);
        packet[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        packet[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        packet[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        packet[12] = self.sbc_frames;
        let mut end = HEADER;
        for frame in stereo_pcm.chunks_exact(FRAMES_PER_SBC * 2) {
            let mut samples = [0i16; FRAMES_PER_SBC * 2];
            for (dst, src) in samples.iter_mut().zip(frame) {
                *dst = (src * 32768.0).round() as i16;
            }
            let progress = self
                .encoder
                .encode(&samples, &mut packet[end..])
                .map_err(|_| invalid("SBC encoding failed"))?;
            end += progress.produced;
        }
        self.channels.send_media(&packet[..end])?;
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self
            .timestamp
            .wrapping_add(u32::from(self.sbc_frames) * 128);
        Ok(())
    }

    pub fn close(mut self) -> io::Result<()> {
        self.signalling
            .accept(&mut self.channels, CLOSE, &[self.seid])?;
        self.channels.close_media()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Peer {
        offers: Vec<u8>,
        responses: Vec<Vec<u8>>,
        commands: Vec<Vec<u8>>,
        mtu: u16,
        media: Vec<Vec<u8>>,
    }

    impl Peer {
        fn stereo() -> Self {
            Self {
                offers: vec![4, 8],
                responses: vec![vec![1, 0, 7, 6, 0, 0, 0x22, 0x15, 2, 35]],
                commands: Vec::new(),
                mtu: 200,
                media: Vec::new(),
            }
        }
    }

    impl Channels for &mut Peer {
        fn exchange(&mut self, cmd: &[u8], reply: &mut [u8; MAX_SDU]) -> io::Result<usize> {
            self.commands.push(cmd.to_vec());
            reply[..2].copy_from_slice(&[cmd[0] | 2, cmd[1]]);
            let body = match cmd[1] {
                1 => self.offers.clone(),
                2 => self.responses.remove(0),
                _ => Vec::new(),
            };
            reply[2..2 + body.len()].copy_from_slice(&body);
            Ok(body.len() + 2)
        }
        fn open_media(&mut self) -> io::Result<u16> {
            Ok(self.mtu)
        }
        fn close_media(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn send_media(&mut self, packet: &[u8]) -> io::Result<()> {
            self.media.push(packet.to_vec());
            Ok(())
        }
    }

    #[test]
    fn selects_available_sink_and_fits_lower_rate_and_pool_to_media_mtu() {
        let mut peer = Peer::stereo();
        // A source, an in-use sink, a sink without SBC and a 44.1 kHz sink.
        peer.offers = vec![4, 0, 10, 8, 12, 8, 16, 8];
        peer.responses.insert(0, vec![1, 0, 7, 2, 0, 2]);
        let mut stream = Stream::start(&mut peer).unwrap();
        assert_eq!(stream.rate(), 44_100);
        assert_eq!(
            stream.channels.commands[3],
            [0x30, 3, 16, 4, 1, 0, 7, 6, 0, 0, 0x22, 0x15, 35, 35]
        );
        assert_eq!(stream.pcm_frames(), 256);
        stream.sequence = u16::MAX;
        stream.timestamp = u32::MAX - 127;
        assert!(stream.send(&[0.0; 2]).is_err());
        for _ in 0..2 {
            stream.send(&[0.0; 512]).unwrap();
        }
        assert_eq!(stream.channels.media.len(), 2);
        assert!(stream
            .channels
            .media
            .iter()
            .all(|p| p.len() <= 200 && p[12] == 2));
        assert_eq!(&stream.channels.media[1][2..8], &[0, 0, 0, 0, 0, 128]);
        stream.close().unwrap();
        assert_eq!(peer.commands.last().unwrap()[1..], [8, 16]);
    }

    #[test]
    fn fails_before_start_on_malformed_incompatible_or_oversized_offers() {
        for caps in [
            vec![1, 0, 7],
            vec![1, 0, 7, 6, 0],                        // truncated
            vec![7, 6, 0, 0, 0x11, 0x15, 2, 51],        // no transport
            vec![1, 0, 7, 6, 0, 0, 0x11, 0x15, 52, 51], // backwards range
            vec![1, 0, 7, 6, 0, 0, 0x11, 0x11, 2, 51],  // no 8-subband mode
        ] {
            let mut peer = Peer::stereo();
            peer.responses = vec![caps];
            assert!(Stream::start(&mut peer).is_err());
            assert_eq!(peer.commands.len(), 2);
        }
        let mut peer = Peer::stereo();
        peer.offers = vec![4; 126];
        assert!(Stream::start(&mut peer).is_err());
        assert_eq!(peer.commands.len(), 1);
        let mut peer = Peer::stereo();
        peer.mtu = 48;
        assert!(Stream::start(&mut peer).is_err());
        assert_eq!(peer.commands.last().unwrap()[1], 6);
    }

    #[test]
    fn refuses_wrong_transaction_rejection_and_fragmentation() {
        struct Response([u8; 2]);
        impl Channels for Response {
            fn exchange(&mut self, _: &[u8], out: &mut [u8; MAX_SDU]) -> io::Result<usize> {
                out[..2].copy_from_slice(&self.0);
                Ok(2)
            }
            fn open_media(&mut self) -> io::Result<u16> {
                unreachable!()
            }
            fn close_media(&mut self) -> io::Result<()> {
                Ok(())
            }
            fn send_media(&mut self, _: &[u8]) -> io::Result<()> {
                unreachable!()
            }
        }
        for reply in [[0x12, 1], [3, 1], [6, 1], [2, 2]] {
            let mut signalling = Signalling { label: 0 };
            assert!(signalling.accept(&mut Response(reply), 1, &[]).is_err());
        }
    }
}
