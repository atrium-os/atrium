#![no_main]
//! Hostile raster streams: the readers never panic or allocate past the limit.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = prelum_raster::read_pwg(data, 1 << 22);
    let _ = prelum_raster::read_urf(data, 1 << 22);
});
