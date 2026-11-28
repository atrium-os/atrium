//! Mutate the sink's signalling responses while keeping a path to each stage.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io;
use vinculum_audio::{Channels, Stream, MAX_SDU};

struct Sink<'a> {
    edits: &'a [u8],
    mtu: u16,
}

impl Channels for Sink<'_> {
    fn exchange(&mut self, command: &[u8], reply: &mut [u8; MAX_SDU]) -> io::Result<usize> {
        let mut response = vec![command[0] | 2, command[1]];
        match command[1] {
            1 => response.extend_from_slice(&[4, 8]),
            2 => response.extend_from_slice(&[1, 0, 7, 6, 0, 0, 0x11, 0x15, 2, 51]),
            _ => {}
        }
        // Each exchange gets one operation. Preserve, replace the body,
        // or replace the entire SDU (including transaction and signal ID).
        if let Some((&op, rest)) = self.edits.split_first() {
            self.edits = rest;
            if op % 3 != 0 {
                let count = usize::from(op / 3).min(rest.len());
                let (edit, rest) = rest.split_at(count);
                self.edits = rest;
                response.truncate(if op % 3 == 1 { 2 } else { 0 });
                response.extend_from_slice(edit);
            }
        }
        reply[..response.len()].copy_from_slice(&response);
        Ok(response.len())
    }
    fn open_media(&mut self) -> io::Result<u16> {
        Ok(self.mtu)
    }
    fn send_media(&mut self, packet: &[u8]) -> io::Result<()> {
        assert!(packet.len() <= MAX_SDU && packet.len() <= usize::from(self.mtu));
        Ok(())
    }
    fn close_media(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let (mtu, edits) = match data {
        [a, b, tail @ ..] => (u16::from_le_bytes([*a, *b]), tail),
        _ => (672, data),
    };
    let sink = Sink { mtu, edits };
    if let Ok(mut stream) = Stream::start(sink) {
        let samples = vec![0.0; 2 * stream.pcm_frames()];
        stream.send(&samples).unwrap();
        let _ = stream.close();
    }
});
