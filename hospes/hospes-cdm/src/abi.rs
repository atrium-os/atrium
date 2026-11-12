//! The Chromium CDM interface, version 10, at the binary level (docs/spec/hospes.md §8.1).
//!
//! `content_decryption_module.h` is C++: the CDM is an object whose first word points at a
//! vtable, and so are the objects we hand it (the host, buffers, frames, file handles).
//! Itanium C++ ABI on both machines: virtual functions in declaration order, `this` first;
//! a virtual destructor takes two slots (complete, deleting) where it is declared; the
//! object's vtable pointer points just past `offset_to_top` and the RTTI pointer.
//!
//! Every layout here is checked by the test CDM (§11.3), which a C++ compiler built from the
//! real header: a mistake shows up as a wrong transcript.
#![allow(dead_code)]

use core::ffi::c_void;

// ---- plain structs (CHECK_TYPE sizes in the header, 64-bit) ----

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SubsampleEntry {
    pub clear_bytes: u32,
    pub cipher_bytes: u32,
}

#[repr(C)]
pub struct InputBuffer2 {
    pub data: *const u8,
    pub data_size: u32,
    pub encryption_scheme: u32,
    pub key_id: *const u8,
    pub key_id_size: u32,
    pub _pad0: u32,
    pub iv: *const u8,
    pub iv_size: u32,
    pub _pad1: u32,
    pub subsamples: *const SubsampleEntry,
    pub num_subsamples: u32,
    pub _pad2: u32,
    pub crypt_byte_block: u32,
    pub skip_byte_block: u32,
    pub timestamp: i64,
}

#[repr(C)]
pub struct AudioDecoderConfig2 {
    pub codec: u32,
    pub channel_count: i32,
    pub bits_per_channel: i32,
    pub samples_per_second: i32,
    pub extra_data: *const u8,
    pub extra_data_size: u32,
    pub encryption_scheme: u32,
}

#[repr(C)]
pub struct VideoDecoderConfig2 {
    pub codec: u32,
    pub profile: u32,
    pub format: u32,
    pub _pad: u32,
    pub width: i32,
    pub height: i32,
    pub extra_data: *const u8,
    pub extra_data_size: u32,
    pub encryption_scheme: u32,
}

#[repr(C)]
pub struct KeyInformation {
    pub key_id: *const u8,
    pub key_id_size: u32,
    pub status: u32,
    pub system_code: u32,
}

#[repr(C)]
pub struct PlatformChallengeResponse {
    pub signed_data: *const u8,
    pub signed_data_length: u32,
    pub signed_data_signature: *const u8,
    pub signed_data_signature_length: u32,
    pub platform_key_certificate: *const u8,
    pub platform_key_certificate_length: u32,
}

#[repr(C)]
pub struct Policy {
    pub min_hdcp_version: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

/// What a C++ vtable pointer points at: the slots, preceded by `offset_to_top` and RTTI.
#[repr(C)]
pub struct VTable<const N: usize> {
    pub offset_to_top: isize,
    pub rtti: *const c_void,
    pub slots: [*const (); N],
}
// Safety: read-only tables of function addresses.
unsafe impl<const N: usize> Sync for VTable<N> {}

/// Any C++ object: its first word is the vtable pointer.
#[repr(C)]
pub struct Obj {
    pub vptr: *const usize,
}

/// Slot `i` of `obj`'s vtable.
///
/// # Safety
/// `obj` must be a live C++ object with at least `i + 1` virtual functions.
pub unsafe fn slot(obj: *mut Obj, i: usize) -> usize {
    *(*obj).vptr.add(i)
}

// ---- ContentDecryptionModule_10's slots ----
pub mod cdm10 {
    pub const INITIALIZE: usize = 0;
    pub const GET_STATUS_FOR_POLICY: usize = 1;
    pub const SET_SERVER_CERTIFICATE: usize = 2;
    pub const CREATE_SESSION: usize = 3;
    pub const LOAD_SESSION: usize = 4;
    pub const UPDATE_SESSION: usize = 5;
    pub const CLOSE_SESSION: usize = 6;
    pub const REMOVE_SESSION: usize = 7;
    pub const TIMER_EXPIRED: usize = 8;
    pub const DECRYPT: usize = 9;
    pub const INIT_AUDIO: usize = 10;
    pub const INIT_VIDEO: usize = 11;
    pub const DEINIT_DECODER: usize = 12;
    pub const RESET_DECODER: usize = 13;
    pub const DECRYPT_AND_DECODE_FRAME: usize = 14;
    pub const DECRYPT_AND_DECODE_SAMPLES: usize = 15;
    pub const ON_PLATFORM_CHALLENGE_RESPONSE: usize = 16;
    pub const ON_QUERY_OUTPUT_PROTECTION_STATUS: usize = 17;
    pub const ON_STORAGE_ID: usize = 18;
    pub const DESTROY: usize = 19;
}

// ---- FileIOClient's slots (the CDM implements it) ----
pub mod file_client {
    pub const ON_OPEN_COMPLETE: usize = 0;
    pub const ON_READ_COMPLETE: usize = 1;
    pub const ON_WRITE_COMPLETE: usize = 2;
}

/// The CDM instance, with typed calls into its vtable.
pub struct Cdm(pub *mut Obj);

macro_rules! call {
    ($self:ident, $slot:expr, fn($($t:ty),*) $(-> $r:ty)? $(, $a:expr)* $(,)?) => {{
        // Safety: the slot has this signature in content_decryption_module.h (version 10).
        let f: extern "C" fn(*mut Obj $(, $t)*) $(-> $r)? = unsafe { core::mem::transmute(slot($self.0, $slot)) };
        f($self.0 $(, $a)*)
    }};
}

impl Cdm {
    pub fn initialize(&self, distinctive: bool, persistent: bool, hw_secure: bool) {
        call!(self, cdm10::INITIALIZE, fn(bool, bool, bool), distinctive, persistent, hw_secure)
    }
    pub fn get_status_for_policy(&self, promise: u32, p: &Policy) {
        call!(self, cdm10::GET_STATUS_FOR_POLICY, fn(u32, *const Policy), promise, p)
    }
    pub fn set_server_certificate(&self, promise: u32, c: &[u8]) {
        call!(self, cdm10::SET_SERVER_CERTIFICATE, fn(u32, *const u8, u32), promise, c.as_ptr(), c.len() as u32)
    }
    pub fn create_session(&self, promise: u32, ty: u32, init_ty: u32, init: &[u8]) {
        call!(self, cdm10::CREATE_SESSION, fn(u32, u32, u32, *const u8, u32), promise, ty, init_ty, init.as_ptr(), init.len() as u32)
    }
    pub fn load_session(&self, promise: u32, ty: u32, id: &[u8]) {
        call!(self, cdm10::LOAD_SESSION, fn(u32, u32, *const u8, u32), promise, ty, id.as_ptr(), id.len() as u32)
    }
    pub fn update_session(&self, promise: u32, id: &[u8], resp: &[u8]) {
        call!(
            self,
            cdm10::UPDATE_SESSION,
            fn(u32, *const u8, u32, *const u8, u32),
            promise,
            id.as_ptr(),
            id.len() as u32,
            resp.as_ptr(),
            resp.len() as u32
        )
    }
    pub fn close_session(&self, promise: u32, id: &[u8]) {
        call!(self, cdm10::CLOSE_SESSION, fn(u32, *const u8, u32), promise, id.as_ptr(), id.len() as u32)
    }
    pub fn remove_session(&self, promise: u32, id: &[u8]) {
        call!(self, cdm10::REMOVE_SESSION, fn(u32, *const u8, u32), promise, id.as_ptr(), id.len() as u32)
    }
    pub fn timer_expired(&self, ctx: usize) {
        call!(self, cdm10::TIMER_EXPIRED, fn(usize), ctx)
    }
    pub fn decrypt(&self, input: &InputBuffer2, out: *mut Obj) -> u32 {
        call!(self, cdm10::DECRYPT, fn(*const InputBuffer2, *mut Obj) -> u32, input, out)
    }
    pub fn init_audio(&self, c: &AudioDecoderConfig2) -> u32 {
        call!(self, cdm10::INIT_AUDIO, fn(*const AudioDecoderConfig2) -> u32, c)
    }
    pub fn init_video(&self, c: &VideoDecoderConfig2) -> u32 {
        call!(self, cdm10::INIT_VIDEO, fn(*const VideoDecoderConfig2) -> u32, c)
    }
    pub fn deinit_decoder(&self, stream: u32) {
        call!(self, cdm10::DEINIT_DECODER, fn(u32), stream)
    }
    pub fn reset_decoder(&self, stream: u32) {
        call!(self, cdm10::RESET_DECODER, fn(u32), stream)
    }
    pub fn decrypt_and_decode_frame(&self, input: &InputBuffer2, frame: *mut Obj) -> u32 {
        call!(self, cdm10::DECRYPT_AND_DECODE_FRAME, fn(*const InputBuffer2, *mut Obj) -> u32, input, frame)
    }
    pub fn on_platform_challenge_response(&self, r: &PlatformChallengeResponse) {
        call!(self, cdm10::ON_PLATFORM_CHALLENGE_RESPONSE, fn(*const PlatformChallengeResponse), r)
    }
    pub fn on_query_output_protection_status(&self, result: u32, link_mask: u32, protection: u32) {
        call!(self, cdm10::ON_QUERY_OUTPUT_PROTECTION_STATUS, fn(u32, u32, u32), result, link_mask, protection)
    }
    pub fn on_storage_id(&self, version: u32, id: &[u8]) {
        call!(self, cdm10::ON_STORAGE_ID, fn(u32, *const u8, u32), version, id.as_ptr(), id.len() as u32)
    }
    pub fn destroy(&self) {
        call!(self, cdm10::DESTROY, fn())
    }
}

/// A FileIOClient the CDM gave us.
pub struct FileClient(pub *mut Obj);

impl FileClient {
    pub fn on_open_complete(&self, status: u32) {
        call!(self, file_client::ON_OPEN_COMPLETE, fn(u32), status)
    }
    pub fn on_read_complete(&self, status: u32, d: &[u8]) {
        call!(self, file_client::ON_READ_COMPLETE, fn(u32, *const u8, u32), status, d.as_ptr(), d.len() as u32)
    }
    pub fn on_write_complete(&self, status: u32) {
        call!(self, file_client::ON_WRITE_COMPLETE, fn(u32), status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    // The header's CHECK_TYPE sizes for 64-bit, and the offsets its padding implies.
    #[test]
    fn layouts() {
        assert_eq!(size_of::<SubsampleEntry>(), 8);
        assert_eq!(size_of::<InputBuffer2>(), 80);
        assert_eq!(offset_of!(InputBuffer2, encryption_scheme), 12);
        assert_eq!(offset_of!(InputBuffer2, key_id), 16);
        assert_eq!(offset_of!(InputBuffer2, iv), 32);
        assert_eq!(offset_of!(InputBuffer2, subsamples), 48);
        assert_eq!(offset_of!(InputBuffer2, crypt_byte_block), 64);
        assert_eq!(offset_of!(InputBuffer2, timestamp), 72);
        assert_eq!(size_of::<AudioDecoderConfig2>(), 32);
        assert_eq!(offset_of!(AudioDecoderConfig2, extra_data), 16);
        assert_eq!(size_of::<VideoDecoderConfig2>(), 40);
        assert_eq!(offset_of!(VideoDecoderConfig2, width), 16);
        assert_eq!(offset_of!(VideoDecoderConfig2, extra_data), 24);
        assert_eq!(offset_of!(VideoDecoderConfig2, encryption_scheme), 36);
        assert_eq!(size_of::<KeyInformation>(), 24);
        assert_eq!(size_of::<PlatformChallengeResponse>(), 48);
        assert_eq!(size_of::<Size>(), 8);
    }
}
