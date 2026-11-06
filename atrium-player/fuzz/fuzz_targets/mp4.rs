//! Push hostile bytes through the mp4 parser in a few chunk sizes, serving
//! its seek requests from the same bytes. It must return, never panic, hang
//! or allocate past its limits.
#![no_main]
use atrium_media_demux::{open, Demuxer, Format, Limits};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&k, data)) = data.split_first() else { return };
    let chunk = 1 + (k as usize * 37) % 4096;
    let mut d = open(Format::Mp4, Limits { max_buffered: 4 << 20, max_frame: 1 << 20, max_index: 100_000, ..Limits::default() });
    let mut at = 0usize;
    let mut seeks = 0;
    for _ in 0..100_000 {
        match d.poll() {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(_) => return,
        }
        if let Some(off) = d.wants_offset() {
            seeks += 1;
            if seeks > 16 || off as usize > data.len() {
                return;
            }
            at = off as usize;
            d.input_at(off);
            continue;
        }
        if at >= data.len() {
            if k & 1 == 1 && d.seek(500_000_000) {
                if let Some(off) = d.wants_offset() {
                    if (off as usize) < data.len() {
                        at = off as usize;
                        d.input_at(off);
                        continue;
                    }
                }
            }
            let _ = d.finish();
            return;
        }
        let end = (at + chunk).min(data.len());
        d.push(&data[at..end]);
        at = end;
    }
});
