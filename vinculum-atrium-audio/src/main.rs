//! The jailed A2DP source. The control socket is one authorized signalling
//! channel. The broker passes a dedicated datagram fd when the media channel opens.
use lyra_protocol::{fdpass, peer, wire::OutputKind};
use std::{
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixDatagram, UnixStream},
    },
    time::Duration,
};
use vinculum_atrium_audio::Output;
use vinculum_audio::{Channels, Stream, MAX_SDU};
use vinculum_control::{
    audio::{FromHost, ToHost},
    encode, Reader, MAX_MESSAGE,
};

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid audio channel reply")
}
struct Link {
    control: UnixStream,
    media: Option<UnixDatagram>,
}
impl Link {
    fn receive(&mut self) -> io::Result<FromHost> {
        // Read precisely one frame: the descriptor rides on the next byte.
        let mut header = [0; 4];
        self.control.read_exact(&mut header)?;
        let len = usize::try_from(u32::from_le_bytes(header)).map_err(|_| bad())?;
        if len > MAX_MESSAGE {
            return Err(bad());
        }
        let mut body = vec![0; len];
        self.control.read_exact(&mut body)?;
        let mut reader = Reader::default();
        reader.push(&header);
        reader.push(&body);
        reader.next().map_err(|_| bad())?.ok_or_else(bad)
    }
    fn send(&mut self, msg: ToHost) -> io::Result<()> {
        self.control.write_all(&encode(&msg))
    }
}
impl Channels for Link {
    fn exchange(&mut self, cmd: &[u8], reply: &mut [u8; MAX_SDU]) -> io::Result<usize> {
        self.send(ToHost::Signal(cmd.to_vec()))?;
        let FromHost::Signal(bytes) = self.receive()? else {
            return Err(bad());
        };
        reply
            .get_mut(..bytes.len())
            .ok_or_else(bad)?
            .copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn open_media(&mut self) -> io::Result<u16> {
        self.send(ToHost::OpenMedia)?;
        let FromHost::MediaReady { mtu } = self.receive()? else {
            return Err(bad());
        };
        let socket = UnixDatagram::from(fdpass::recv_fd(self.control.as_raw_fd())?);
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        self.media = Some(socket);
        Ok(mtu)
    }
    fn close_media(&mut self) -> io::Result<()> {
        self.send(ToHost::CloseMedia)?;
        if !matches!(self.receive()?, FromHost::MediaClosed) {
            return Err(bad());
        }
        self.media = None;
        Ok(())
    }
    fn send_media(&mut self, packet: &[u8]) -> io::Result<()> {
        let sent = self.media.as_ref().ok_or_else(bad)?.send(packet)?;
        if sent != packet.len() {
            return Err(bad());
        }
        Ok(())
    }
}
fn run(host: &str, uid: u32, device: &str) -> io::Result<()> {
    let control = UnixStream::connect(host)?;
    if peer::peer_uid(control.as_raw_fd())? != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wrong vinculumd uid",
        ));
    }
    let mut link = Link {
        control,
        media: None,
    };
    let FromHost::Attach { id, name } = link.receive()? else {
        return Err(bad());
    };
    link.control
        .set_read_timeout(Some(Duration::from_secs(10)))?;
    link.control
        .set_write_timeout(Some(Duration::from_secs(1)))?;
    let stream = Stream::start(link)?;
    eprintln!(
        "vinculum-audio: authorized output {name:?}, {} Hz",
        stream.rate()
    );
    let mut output = Output::connect(device, id, name, OutputKind::Headphones, stream, 0)?;
    loop {
        output.pump()?;
    }
}
fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let [_, host, uid, device] = args.as_slice() else {
        return Err(io::Error::other(
            "usage: vinculum-audio BLUETOOTH_SOCKET HOST_UID AUDIO_DEVICE_SOCKET",
        ));
    };
    let uid = uid
        .parse()
        .map_err(|_| io::Error::other("invalid host uid"))?;
    loop {
        if let Err(e) = run(host, uid, device) {
            eprintln!("vinculum-audio: {e}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
