//! The tools' crypto backend for sealed volumes (crypt.h's
//! tessera_crypt_backend_t), on RustCrypto: AES-256 blocks for XAES's key
//! derivation, AES-256-GCM and ChaCha20-Poly1305 with 96-bit nonces, and the
//! OS random source. The C core builds XAES-256-GCM, XChaCha20-Poly1305 and
//! everything sealed on these; nothing here knows the disk format.

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use aes::Aes256;
use aes_gcm::aead::AeadInPlace;
use aes_gcm::Aes256Gcm;
use chacha20poly1305::ChaCha20Poly1305;
use core::ffi::c_int;
use std::sync::Once;

use tessera_sys::*;

const OK: c_int = 0;
const EINVAL: c_int = -1;
const EAUTH: c_int = -25;
const EINCOMPAT: c_int = -12;

unsafe fn key32<'a>(k: *const u8) -> &'a [u8; 32] { &*(k as *const [u8; 32]) }

unsafe extern "C" fn aes256_ecb(key: *const u8, input: *const u8, out: *mut u8,
                                nblocks: usize) -> c_int {
    if key.is_null() || (nblocks > 0 && (input.is_null() || out.is_null())) {
        return EINVAL;
    }
    let c = Aes256::new(GenericArray::from_slice(key32(key)));
    for i in 0..nblocks {
        let mut b = GenericArray::clone_from_slice(
            std::slice::from_raw_parts(input.add(i * 16), 16));
        c.encrypt_block(&mut b);
        std::ptr::copy_nonoverlapping(b.as_ptr(), out.add(i * 16), 16);
    }
    OK
}

/// A buffer of `len` bytes at `dst` holding a copy of `src` (which may be
/// the same memory), as a slice to work on in place.
unsafe fn in_place<'a>(src: *const u8, dst: *mut u8, len: usize) -> &'a mut [u8] {
    if len == 0 { return &mut []; }
    if src != dst as *const u8 { std::ptr::copy(src, dst, len); }
    std::slice::from_raw_parts_mut(dst, len)
}

unsafe fn slice<'a>(p: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { std::slice::from_raw_parts(p, len) }
}

unsafe extern "C" fn aead96_seal(alg: u32, key: *const u8, nonce: *const u8,
                                 aad: *const u8, aad_len: usize, pt: *const u8,
                                 len: usize, ct: *mut u8, tag: *mut u8) -> c_int {
    if key.is_null() || nonce.is_null() || tag.is_null() { return EINVAL; }
    let n = GenericArray::from_slice(slice(nonce, 12));
    let a = slice(aad, aad_len);
    let buf = in_place(pt, ct, len);
    let t = match alg {
        TESSERA_AEAD96_AES_256_GCM => Aes256Gcm::new(GenericArray::from_slice(key32(key)))
            .encrypt_in_place_detached(n, a, buf),
        TESSERA_AEAD96_CHACHA20_POLY1305 =>
            ChaCha20Poly1305::new(GenericArray::from_slice(key32(key)))
                .encrypt_in_place_detached(n, a, buf),
        _ => return EINCOMPAT,
    };
    match t {
        Ok(t) => { std::ptr::copy_nonoverlapping(t.as_ptr(), tag, 16); OK }
        Err(_) => EINVAL,
    }
}

unsafe extern "C" fn aead96_open(alg: u32, key: *const u8, nonce: *const u8,
                                 aad: *const u8, aad_len: usize, ct: *const u8,
                                 len: usize, tag: *const u8, pt: *mut u8) -> c_int {
    if key.is_null() || nonce.is_null() || tag.is_null() { return EINVAL; }
    let n = GenericArray::from_slice(slice(nonce, 12));
    let a = slice(aad, aad_len);
    let t = GenericArray::from_slice(slice(tag, 16));
    let buf = in_place(ct, pt, len);
    let r = match alg {
        TESSERA_AEAD96_AES_256_GCM => Aes256Gcm::new(GenericArray::from_slice(key32(key)))
            .decrypt_in_place_detached(n, a, buf, t),
        TESSERA_AEAD96_CHACHA20_POLY1305 =>
            ChaCha20Poly1305::new(GenericArray::from_slice(key32(key)))
                .decrypt_in_place_detached(n, a, buf, t),
        _ => return EINCOMPAT,
    };
    if r.is_err() {
        buf.fill(0);
        return EAUTH;
    }
    OK
}

unsafe extern "C" fn random(buf: *mut u8, len: usize) {
    if len == 0 { return; }
    let b = std::slice::from_raw_parts_mut(buf, len);
    getrandom::getrandom(b).expect("tessera: no OS random source");
}

static BACKEND: tessera_crypt_backend_t = tessera_crypt_backend_t {
    aes256_ecb: Some(aes256_ecb),
    aead96_seal: Some(aead96_seal),
    aead96_open: Some(aead96_open),
    random: Some(random),
};

/// Install the backend into the C core (idempotent).
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe { tessera_crypt_set_backend(&BACKEND) });
}
