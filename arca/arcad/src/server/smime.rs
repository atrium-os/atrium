//! Arca-backed RSA-OAEP CMS key transport. Mail identifies a certificate
//! key by its public SPKI hash. Arca never accepts a persona from Mail.

use super::*;
use crate::proto::{SMIME_CERTS_MAX, SMIME_CERT_MAX};

pub(super) const MAIL: &str = "org.atrium.mail";
const MAX_CIPHERTEXT: usize = 512;

fn storage_persona(persona: Option<&str>) -> Option<&str> {
    match persona {
        None => Some("personal"),
        Some(name) if name != PERSONA && portcullis_peer::valid_person_persona(name) => Some(name),
        Some(_) => None,
    }
}

impl State {
    // Trusted UI only. The supplied PKCS#8 bytes arrive in Arca's locked
    // request buffer and are sealed directly into the unlocked user's store.
    pub(super) fn smime_import(
        &mut self,
        body: &[u8],
        item: &mut Option<ItemId>,
        paired: bool,
    ) -> Reply {
        let mut r = Reader::new(body);
        let (Ok(user), Ok(persona), Ok(label)) = (
            r.str(caps::USER),
            r.str(caps::PERSONA),
            r.str(caps::ACCOUNT),
        ) else {
            return malformed();
        };
        let cert = if paired {
            match r.bytes(SMIME_CERT_MAX) {
                Ok(cert) if !cert.is_empty() => cert,
                _ => return malformed(),
            }
        } else {
            &[]
        };
        let Ok(der) = r.bytes(caps::VALUE) else {
            return malformed();
        };
        if r.end().is_err() || label.trim().is_empty() || label.chars().any(char::is_control) {
            return malformed();
        }
        let Some(persona) = storage_persona((!persona.is_empty()).then_some(persona)) else {
            return malformed();
        };
        let Some(publisher) = self
            .mail_sign_ins
            .mail_publisher
            .filter(|p| *p != crate::label::UNSIGNED_PUBLISHER)
        else {
            return reply(status::UNSUPPORTED);
        };
        let Some(store) = self
            .stores
            .get_mut(user)
            .filter(|s| s.state() == LockState::Unlocked)
        else {
            return reply(status::LOCKED);
        };
        if !native::AVAILABLE {
            return reply(status::UNSUPPORTED);
        }
        let key = tri!(native::Key::parse(der));
        tri!(key.check_private());
        let fingerprint = tri!(key.fingerprint());
        let recipient = Recipient::Smime { publisher };
        let access = Access {
            persona,
            recipient: &recipient,
        };
        let items = tri!(store.list(&access));
        let existing = items
            .iter()
            .find(|m| m.kind == Kind::SmimeRsaKey && m.scope.starts_with(&fingerprint));
        if paired && tri!(native::certificate_fingerprint(cert)) != fingerprint {
            return malformed();
        }
        if existing.is_some_and(|m| !paired || certificate(m).is_some()) {
            return reply(status::EXISTS);
        }
        if paired && items.iter().filter(|m| certificate(m).is_some()).count() >= SMIME_CERTS_MAX {
            return reply(status::FULL);
        }
        let mut scope = fingerprint.to_vec();
        scope.extend_from_slice(cert);
        if let Some(m) = existing {
            // A provisioned bare key can gain its matching certificate.
            // The sealed key and its item ID stay the same.
            *item = Some(m.id);
            tri!(store.set_scope(&m.id, &access, &scope));
            return ok(fingerprint.to_vec());
        }
        let saved = tri!(store.add(NewItem {
            persona,
            recipient,
            class: Class::WhileUnlocked,
            kind: Kind::SmimeRsaKey,
            provenance: Provenance::Migrated,
            account: label,
            scope: &scope,
            value: der,
        }));
        *item = Some(saved.id);
        ok(fingerprint.to_vec())
    }

    pub(super) fn smime_forget(&mut self, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let mut r = Reader::new(body);
        let (Ok(user), Ok(persona), Ok(fingerprint)) =
            (r.str(caps::USER), r.str(caps::PERSONA), r.array::<32>())
        else {
            return malformed();
        };
        if r.end().is_err() {
            return malformed();
        }
        let Some(persona) = storage_persona((!persona.is_empty()).then_some(persona)) else {
            return malformed();
        };
        let Some(publisher) = self.mail_sign_ins.mail_publisher else {
            return reply(status::DENIED);
        };
        let Some(store) = self
            .stores
            .get_mut(user)
            .filter(|s| s.state() == LockState::Unlocked)
        else {
            return reply(status::LOCKED);
        };
        let recipient = Recipient::Smime { publisher };
        let access = Access {
            persona,
            recipient: &recipient,
        };
        let Some(id) = tri!(key_id(store, &access, &fingerprint)) else {
            return reply(status::DENIED);
        };
        *item = Some(id);
        tri!(store.delete(&id, &access));
        ok(Vec::new())
    }

    pub(super) fn smime_certs(&self, user: &str, persona: Option<&str>, body: &[u8]) -> Reply {
        if !body.is_empty() {
            return malformed();
        }
        let Some(persona) = storage_persona(persona) else {
            return reply(status::DENIED);
        };
        let Some(store) = self
            .stores
            .get(user)
            .filter(|s| s.state() == LockState::Unlocked)
        else {
            return reply(status::LOCKED);
        };
        let Some(publisher) = self.mail_sign_ins.mail_publisher else {
            return reply(status::DENIED);
        };
        let recipient = Recipient::Smime { publisher };
        let items = tri!(store.list(&Access {
            persona,
            recipient: &recipient
        }));
        let certs: Vec<_> = items
            .iter()
            .rev()
            .filter_map(certificate)
            .take(SMIME_CERTS_MAX)
            .collect();
        let mut w = Writer::new();
        w.u32(certs.len() as u32);
        for cert in certs {
            w.bytes(cert);
        }
        ok(w.buf)
    }

    pub(super) fn smime_unwrap(
        &self,
        user: &str,
        persona: Option<&str>,
        body: &[u8],
        item: &mut Option<ItemId>,
    ) -> Reply {
        let mut r = Reader::new(body);
        let (Ok(fingerprint), Ok(ciphertext)) = (r.array::<32>(), r.bytes(MAX_CIPHERTEXT)) else {
            return malformed();
        };
        if r.end().is_err() || ciphertext.len() < 256 {
            return malformed();
        }
        let Some(persona) = storage_persona(persona) else {
            return reply(status::DENIED);
        };
        let Some(store) = self
            .stores
            .get(user)
            .filter(|s| s.state() == LockState::Unlocked)
        else {
            return reply(status::LOCKED);
        };
        let Some(publisher) = self.mail_sign_ins.mail_publisher else {
            return reply(status::DENIED);
        };
        let recipient = Recipient::Smime { publisher };
        let access = Access {
            persona,
            recipient: &recipient,
        };
        let Some(id) = tri!(key_id(store, &access, &fingerprint)) else {
            return reply(status::DENIED);
        };
        *item = Some(id);
        if !native::AVAILABLE {
            return reply(status::UNSUPPORTED);
        }
        let (_, secret) = tri!(store.release(&id, &access, None));
        let key = tri!(native::Key::parse(secret.as_ref()));
        drop(secret);
        let content_key = tri!(key.unwrap(ciphertext));
        Reply {
            status: status::OK,
            body: Body::Secret(content_key),
        }
    }
}

fn key_id(
    store: &Store,
    access: &Access<'_>,
    fingerprint: &[u8; 32],
) -> Result<Option<ItemId>, Error> {
    Ok(store
        .list(access)?
        .into_iter()
        .find(|m| m.kind == Kind::SmimeRsaKey && m.scope.starts_with(fingerprint))
        .map(|m| m.id))
}

// The first 32 bytes remain the public-key fingerprint even for
// keys imported before the optional public certificate was stored.
fn certificate(item: &arca_store::ItemMeta) -> Option<&[u8]> {
    let der = item.scope.get(32..)?;
    (item.kind == Kind::SmimeRsaKey && (1..=SMIME_CERT_MAX).contains(&der.len())).then_some(der)
}

#[cfg(all(test, feature = "legacy-smime-prototype", any(target_os = "freebsd", feature = "host-openssl-tests")))]
mod legacy;

#[cfg(any(target_os = "freebsd", all(test, feature = "host-openssl-tests")))]
mod native {
    use std::ffi::{c_char, c_int, c_long, c_void};
    use std::ptr::{self, NonNull};

    use sha2::{Digest, Sha256};

    use super::*;

    type Allocate = unsafe extern "C" fn(usize, *const c_char, c_int) -> *mut c_void;
    type Resize = unsafe extern "C" fn(*mut c_void, usize, *const c_char, c_int) -> *mut c_void;
    type Free = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);

    // OpenSSL's arithmetic and DER decoder can copy a private key. Put
    // those allocations in Arca's locked, zeroized buffers as well. If an
    // earlier OpenSSL consumer has already set up its heap, fail closed.
    #[repr(C, align(16))]
    struct Allocation {
        mapping: LockedBuf,
    }

    const HEADER: usize = std::mem::size_of::<Allocation>();

    unsafe extern "C" fn allocate(size: usize, _: *const c_char, _: c_int) -> *mut c_void {
        // The native provider uses small allocations for this fixed profile.
        if !(1..=1024 * 1024).contains(&size) {
            return ptr::null_mut();
        }
        let Ok(mut mapping) = LockedBuf::zeroed(HEADER + size) else {
            return ptr::null_mut();
        };
        let head = mapping.as_mut().as_mut_ptr();
        // SAFETY: mmap is aligned for Allocation and the allocation includes
        // the header and size bytes after it. The header owns the live mapping.
        unsafe {
            head.cast::<Allocation>().write(Allocation { mapping });
            head.add(HEADER).cast()
        }
    }

    unsafe extern "C" fn free(data: *mut c_void, _: *const c_char, _: c_int) {
        if !data.is_null() {
            // SAFETY: OpenSSL passes this allocator's live pointers. Move
            // ownership out of the header before zeroing and unmapping it.
            drop(unsafe { data.cast::<u8>().sub(HEADER).cast::<Allocation>().read() });
        }
    }

    unsafe extern "C" fn resize(
        data: *mut c_void,
        size: usize,
        file: *const c_char,
        line: c_int,
    ) -> *mut c_void {
        if data.is_null() {
            return unsafe { allocate(size, file, line) };
        }
        if size == 0 {
            unsafe { free(data, file, line) };
            return ptr::null_mut();
        }
        let new = unsafe { allocate(size, file, line) };
        if !new.is_null() {
            // SAFETY: the old header still owns its mapping. The new block
            // is disjoint, and we copy only the bytes both buffers have.
            unsafe {
                let header = &*data.cast::<u8>().sub(HEADER).cast::<Allocation>();
                let copied = size.min(header.mapping.len() - HEADER);
                ptr::copy_nonoverlapping(data.cast::<u8>(), new.cast::<u8>(), copied);
                free(data, file, line);
            }
        }
        new
    }

    fn require_locked_heap() -> Result<(), Error> {
        static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *INSTALLED
            .get_or_init(|| unsafe { CRYPTO_set_mem_functions(allocate, resize, free) == 1 })
        {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::other(
                "OpenSSL locked allocator unavailable",
            )))
        }
    }

    // OpenSSL EVP on FreeBSD. All native error and key objects are disposed
    // locally. The wire profile selects exactly one padding algorithm.
    #[link(name = "crypto")]
    unsafe extern "C" {
        fn CRYPTO_set_mem_functions(alloc: Allocate, resize: Resize, free: Free) -> c_int;
        fn d2i_X509(out: *mut *mut c_void, input: *mut *const u8, len: c_long) -> *mut c_void;
        fn X509_free(cert: *mut c_void);
        fn X509_get_pubkey(cert: *mut c_void) -> *mut c_void;
        fn X509_check_ca(cert: *mut c_void) -> c_int;
        fn X509_get_key_usage(cert: *mut c_void) -> u32;
        fn d2i_PKCS8_PRIV_KEY_INFO(
            out: *mut *mut c_void,
            input: *mut *const u8,
            len: c_long,
        ) -> *mut c_void;
        fn PKCS8_PRIV_KEY_INFO_free(info: *mut c_void);
        fn EVP_PKCS82PKEY(info: *const c_void) -> *mut c_void;
        fn EVP_PKEY_free(key: *mut c_void);
        fn EVP_PKEY_is_a(key: *const c_void, name: *const c_char) -> c_int;
        fn EVP_PKEY_get_bits(key: *const c_void) -> c_int;
        fn EVP_PKEY_get_size(key: *const c_void) -> c_int;
        fn i2d_PUBKEY(key: *const c_void, out: *mut *mut u8) -> c_int;
        fn EVP_PKEY_CTX_new_from_pkey(
            lib: *mut c_void,
            key: *mut c_void,
            props: *const c_char,
        ) -> *mut c_void;
        fn EVP_PKEY_CTX_free(ctx: *mut c_void);
        fn EVP_PKEY_private_check(ctx: *mut c_void) -> c_int;
        fn EVP_PKEY_decrypt_init(ctx: *mut c_void) -> c_int;
        fn EVP_PKEY_CTX_set_rsa_padding(ctx: *mut c_void, mode: c_int) -> c_int;
        fn EVP_PKEY_CTX_set_rsa_oaep_md_name(
            ctx: *mut c_void,
            md: *const c_char,
            props: *const c_char,
        ) -> c_int;
        fn EVP_PKEY_CTX_set_rsa_mgf1_md_name(
            ctx: *mut c_void,
            md: *const c_char,
            props: *const c_char,
        ) -> c_int;
        fn EVP_PKEY_decrypt(
            ctx: *mut c_void,
            out: *mut u8,
            out_len: *mut usize,
            input: *const u8,
            len: usize,
        ) -> c_int;
        fn ERR_clear_error();
        #[cfg(test)]
        fn EVP_PKEY_encrypt_init(ctx: *mut c_void) -> c_int;
        #[cfg(test)]
        fn EVP_PKEY_encrypt(
            ctx: *mut c_void,
            out: *mut u8,
            out_len: *mut usize,
            input: *const u8,
            len: usize,
        ) -> c_int;
    }

    #[cfg(all(test, feature = "legacy-smime-prototype"))]
    #[repr(C)]
    struct OsslParam {
        key: *const c_char,
        data_type: std::ffi::c_uint,
        data: *mut c_void,
        data_size: usize,
        return_size: usize,
    }

    #[cfg(all(test, feature = "legacy-smime-prototype"))]
    #[link(name = "crypto")]
    unsafe extern "C" {
        fn OSSL_PARAM_construct_uint(key: *const c_char, value: *mut std::ffi::c_uint) -> OsslParam;
        fn OSSL_PARAM_construct_end() -> OsslParam;
        fn EVP_PKEY_CTX_set_params(ctx: *mut c_void, params: *const OsslParam) -> c_int;
        fn EVP_PKEY_CTX_get_params(ctx: *mut c_void, params: *mut OsslParam) -> c_int;
    }

    pub(super) const AVAILABLE: bool = true;
    const RSA_PKCS1_OAEP_PADDING: c_int = 4;
    const INVALID_KEY: Error = Error::BadInput("S/MIME RSA private key");
    const INVALID_CIPHERTEXT: Error = Error::BadInput("S/MIME wrapped key");

    pub(super) struct Key(NonNull<c_void>);
    struct Context(NonNull<c_void>);
    struct Pkcs8(NonNull<c_void>);
    struct ClearErrors;

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { EVP_PKEY_free(self.0.as_ptr()) }
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            unsafe { EVP_PKEY_CTX_free(self.0.as_ptr()) }
        }
    }

    impl Drop for Pkcs8 {
        fn drop(&mut self) {
            unsafe { PKCS8_PRIV_KEY_INFO_free(self.0.as_ptr()) }
        }
    }

    impl Drop for ClearErrors {
        fn drop(&mut self) {
            unsafe { ERR_clear_error() }
        }
    }

    struct Certificate(NonNull<c_void>);
    impl Drop for Certificate {
        fn drop(&mut self) {
            // SAFETY: this owns the pointer returned by d2i_X509.
            unsafe { X509_free(self.0.as_ptr()) }
        }
    }

    pub(super) fn certificate_fingerprint(der: &[u8]) -> Result<[u8; 32], Error> {
        require_locked_heap()?;
        let _errors = ClearErrors;
        const INVALID_CERT: Error = Error::BadInput("S/MIME certificate");
        let mut input = der.as_ptr();
        // SAFETY: the decoder may consume only the provided bounded input.
        let cert = Certificate(
            NonNull::new(unsafe { d2i_X509(ptr::null_mut(), &mut input, der.len() as c_long) })
                .ok_or(INVALID_CERT)?,
        );
        const KEY_ENCIPHERMENT: u32 = 0x20;
        if input != der.as_ptr().wrapping_add(der.len())
            || unsafe {
                X509_check_ca(cert.0.as_ptr()) != 0
                    || X509_get_key_usage(cert.0.as_ptr()) & KEY_ENCIPHERMENT == 0
            }
        {
            return Err(INVALID_CERT);
        }
        let key =
            Key(NonNull::new(unsafe { X509_get_pubkey(cert.0.as_ptr()) }).ok_or(INVALID_CERT)?);
        if unsafe { EVP_PKEY_is_a(key.0.as_ptr(), c"RSA".as_ptr()) } != 1 {
            return Err(INVALID_CERT);
        }
        key.fingerprint()
    }

    impl Key {
        pub(super) fn parse(der: &[u8]) -> Result<Self, Error> {
            require_locked_heap()?;
            let _errors = ClearErrors;
            let mut input = der.as_ptr();
            // EVP owns its normalized key. Neither OpenSSL nor Arca returns
            // either representation to the caller.
            let info = Pkcs8(
                NonNull::new(unsafe {
                    d2i_PKCS8_PRIV_KEY_INFO(
                        ptr::null_mut(),
                        &mut input,
                        der.len().try_into().map_err(|_| INVALID_KEY)?,
                    )
                })
                .ok_or(INVALID_KEY)?,
            );
            if input != der.as_ptr().wrapping_add(der.len()) {
                return Err(INVALID_KEY);
            }
            let key =
                Self(NonNull::new(unsafe { EVP_PKCS82PKEY(info.0.as_ptr()) }).ok_or(INVALID_KEY)?);
            if unsafe { EVP_PKEY_is_a(key.0.as_ptr(), c"RSA".as_ptr()) } != 1
                || !matches!(
                    unsafe { EVP_PKEY_get_bits(key.0.as_ptr()) },
                    2048 | 3072 | 4096
                )
            {
                return Err(INVALID_KEY);
            }
            Ok(key)
        }

        pub(super) fn check_private(&self) -> Result<(), Error> {
            let _errors = ClearErrors;
            let ctx = self.context()?;
            if unsafe { EVP_PKEY_private_check(ctx.0.as_ptr()) } == 1 {
                Ok(())
            } else {
                Err(INVALID_KEY)
            }
        }

        pub(super) fn fingerprint(&self) -> Result<[u8; 32], Error> {
            let _errors = ClearErrors;
            let n = unsafe { i2d_PUBKEY(self.0.as_ptr(), ptr::null_mut()) };
            if !(1..=1024).contains(&n) {
                return Err(INVALID_KEY);
            }
            let mut spki = vec![0; n as usize];
            if unsafe { i2d_PUBKEY(self.0.as_ptr(), &mut spki.as_mut_ptr()) } != n {
                return Err(INVALID_KEY);
            }
            Ok(Sha256::digest(&spki).into())
        }

        fn context(&self) -> Result<Context, Error> {
            NonNull::new(unsafe {
                EVP_PKEY_CTX_new_from_pkey(
                    ptr::null_mut(),
                    self.0.as_ptr(),
                    c"provider=default".as_ptr(),
                )
            })
            .map(Context)
            .ok_or(INVALID_KEY)
        }

        fn size(&self) -> usize {
            // The key was already checked to be RSA of an allowed size.
            unsafe { EVP_PKEY_get_size(self.0.as_ptr()) as usize }
        }

        pub(super) fn unwrap(&self, ciphertext: &[u8]) -> Result<LockedBuf, Error> {
            let _errors = ClearErrors;
            let mut n = self.size();
            if n != ciphertext.len() {
                return Err(INVALID_CIPHERTEXT);
            }
            let ctx = self.context()?;
            if unsafe { EVP_PKEY_decrypt_init(ctx.0.as_ptr()) } != 1 {
                return Err(INVALID_CIPHERTEXT);
            }
            ctx.set_oaep()?;
            let mut out = LockedBuf::zeroed(n).map_err(Error::Io)?;
            if unsafe {
                EVP_PKEY_decrypt(
                    ctx.0.as_ptr(),
                    out.as_mut().as_mut_ptr(),
                    &mut n,
                    ciphertext.as_ptr(),
                    ciphertext.len(),
                )
            } != 1
                || !matches!(n, 16 | 32)
            {
                return Err(INVALID_CIPHERTEXT);
            }
            LockedBuf::from_slice(&out.as_ref()[..n]).map_err(Error::Io)
        }

        /// Prototype only. The worker receives exactly the cipher's public
        /// key size. The result and recovered length never leave Arca.
        #[cfg(all(test, feature = "legacy-smime-prototype"))]
        pub(super) fn legacy_candidate(&self, ciphertext: &[u8], key_bytes: usize) -> Result<LockedBuf, Error> {
            use subtle::{ConditionallySelectable, ConstantTimeEq};

            if !matches!(key_bytes, 16 | 32) || ciphertext.len() != self.size() {
                return Err(INVALID_CIPHERTEXT);
            }
            let _errors = ClearErrors;
            let ctx = self.context()?;
            ctx.set_legacy_with_implicit_rejection()?;
            let mut n = self.size();
            let mut candidate = LockedBuf::zeroed(n).map_err(Error::Io)?;
            let mut output = LockedBuf::zeroed(key_bytes).map_err(Error::Io)?;
            getrandom::getrandom(output.as_mut())
                .map_err(|_| Error::Io(std::io::Error::other("legacy S/MIME randomness unavailable")))?;
            // SAFETY: the initialized default-provider context and both
            // buffers live through the call. n starts at the output capacity.
            let status = unsafe {
                EVP_PKEY_decrypt(ctx.0.as_ptr(), candidate.as_mut().as_mut_ptr(),
                    &mut n, ciphertext.as_ptr(), ciphertext.len())
            };
            let usable = status.ct_eq(&1) & n.ct_eq(&key_bytes);
            for (fallback, byte) in output.as_mut().iter_mut().zip(candidate.as_ref()) {
                *fallback = u8::conditional_select(fallback, byte, usable);
            }
            Ok(output)
        }

        #[cfg(test)]
        pub(super) fn wrap_for_test(&self, key: &[u8]) -> Vec<u8> {
            let _errors = ClearErrors;
            let ctx = self.context().unwrap();
            assert_eq!(unsafe { EVP_PKEY_encrypt_init(ctx.0.as_ptr()) }, 1);
            ctx.set_oaep().unwrap();
            let mut n = self.size();
            let mut ciphertext = vec![0; n];
            assert_eq!(
                unsafe {
                    EVP_PKEY_encrypt(
                        ctx.0.as_ptr(),
                        ciphertext.as_mut_ptr(),
                        &mut n,
                        key.as_ptr(),
                        key.len(),
                    )
                },
                1
            );
            assert_eq!(n, ciphertext.len());
            ciphertext
        }
    }

    impl Context {
        #[cfg(all(test, feature = "legacy-smime-prototype"))]
        fn set_legacy_with_implicit_rejection(&self) -> Result<(), Error> {
            const RSA_PKCS1_PADDING: c_int = 1;
            let ctx = self.0.as_ptr();
            let mut enabled = 1;
            let mut actual = 0;
            // SAFETY: the parameter arrays are terminated and point to
            // live unsigned values for the duration of the EVP calls.
            let checked = unsafe {
                let setting = [
                    OSSL_PARAM_construct_uint(c"implicit-rejection".as_ptr(), &mut enabled),
                    OSSL_PARAM_construct_end(),
                ];
                let mut reading = [
                    OSSL_PARAM_construct_uint(c"implicit-rejection".as_ptr(), &mut actual),
                    OSSL_PARAM_construct_end(),
                ];
                EVP_PKEY_decrypt_init(ctx) == 1
                    && EVP_PKEY_CTX_set_rsa_padding(ctx, RSA_PKCS1_PADDING) == 1
                    && EVP_PKEY_CTX_set_params(ctx, setting.as_ptr()) == 1
                    && EVP_PKEY_CTX_get_params(ctx, reading.as_mut_ptr()) == 1
            };
            if checked && actual == 1 {
                Ok(())
            } else {
                Err(Error::Io(std::io::Error::other("legacy S/MIME provider protection unavailable")))
            }
        }

        fn set_oaep(&self) -> Result<(), Error> {
            let p = self.0.as_ptr();
            if unsafe {
                EVP_PKEY_CTX_set_rsa_padding(p, RSA_PKCS1_OAEP_PADDING) == 1
                    && EVP_PKEY_CTX_set_rsa_oaep_md_name(p, c"SHA256".as_ptr(), ptr::null()) == 1
                    && EVP_PKEY_CTX_set_rsa_mgf1_md_name(p, c"SHA256".as_ptr(), ptr::null()) == 1
            } {
                Ok(())
            } else {
                Err(INVALID_CIPHERTEXT)
            }
        }
    }
}

#[cfg(not(any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
mod native {
    use super::*;
    pub(super) fn certificate_fingerprint(_: &[u8]) -> Result<[u8; 32], Error> {
        Err(Error::BadInput("S/MIME needs the FreeBSD crypto provider"))
    }
    pub(super) const AVAILABLE: bool = false;
    pub(super) struct Key;
    impl Key {
        pub(super) fn parse(_: &[u8]) -> Result<Self, Error> {
            Err(Error::BadInput("S/MIME needs the FreeBSD crypto provider"))
        }
        pub(super) fn check_private(&self) -> Result<(), Error> {
            unreachable!()
        }
        pub(super) fn fingerprint(&self) -> Result<[u8; 32], Error> {
            unreachable!()
        }
        pub(super) fn unwrap(&self, _: &[u8]) -> Result<LockedBuf, Error> {
            unreachable!()
        }
    }
}

#[cfg(all(test, any(target_os = "freebsd", feature = "host-openssl-tests")))]
mod tests {
    use super::*;

    fn body(build: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut writer = Writer::new();
        build(&mut writer);
        writer.buf
    }

    fn mail(user: &str, publisher: [u8; 32], app_id: &str, persona: Option<&str>) -> Label {
        Label::App {
            user: user.into(),
            publisher,
            app_id: app_id.into(),
            persona: persona.map(str::to_owned),
        }
    }

    #[test]
    fn unwrap_uses_only_the_signed_mail_launch_persona_and_unlocked_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(dir.path().into()).unwrap();
        state.mail_sign_ins.mail_publisher = Some([7; 32]);
        for user in ["alice", "bob"] {
            let create = body(|w| {
                w.str(user);
                w.raw(&[1; 32]);
            });
            assert_eq!(
                state
                    .handle(&Label::Vestibulum, op::V_CREATE, &create)
                    .status,
                status::OK
            );
        }
        let work = mail("alice", [7; 32], MAIL, Some("work"));
        let fixture = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../mail/mail-smime/tests/data/leaf.p8"
        ));
        let import = |der| {
            body(|w| {
                w.str("alice");
                w.str("work");
                w.str("Fixture");
                w.bytes(der);
            })
        };
        assert_eq!(
            state
                .handle(&work, op::U_SMIME_IMPORT, &import(fixture))
                .status,
            status::DENIED
        );
        let mut trailing = fixture.to_vec();
        trailing.push(0);
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_IMPORT, &import(&trailing))
                .status,
            status::MALFORMED
        );
        let enrolled = state.handle(&Label::TrustedUi, op::U_SMIME_IMPORT, &import(fixture));
        assert_eq!(enrolled.status, status::OK);
        let fingerprint = enrolled.body.bytes();
        assert_eq!(fingerprint.len(), 32);
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_IMPORT, &import(fixture))
                .status,
            status::EXISTS
        );

        let native_key = native::Key::parse(fixture).unwrap();
        let request = |hash: &[u8], ct: &[u8]| {
            body(|w| {
                w.raw(hash);
                w.bytes(ct);
            })
        };
        for content in [&[23; 16][..], &[42; 32][..]] {
            let ciphertext = native_key.wrap_for_test(content);
            let unwrap = request(fingerprint, &ciphertext);
            let opened = state.handle(&work, op::MAIL_SMIME_UNWRAP, &unwrap);
            assert_eq!(opened.status, status::OK);
            assert!(matches!(&opened.body, Body::Secret(secret) if secret.ct_eq(content)));
            for peer in [
                mail("alice", [7; 32], MAIL, None),
                mail("alice", [7; 32], MAIL, Some("another")),
                mail("alice", [8; 32], MAIL, Some("work")),
                mail("alice", [0; 32], MAIL, Some("work")),
                mail("alice", [7; 32], "org.example.other", Some("work")),
                mail("bob", [7; 32], MAIL, Some("work")),
                Label::TrustedUi,
            ] {
                assert_eq!(
                    state.handle(&peer, op::MAIL_SMIME_UNWRAP, &unwrap).status,
                    status::DENIED,
                    "{peer:?}"
                );
            }
            assert_eq!(
                state
                    .handle(
                        &work,
                        op::MAIL_SMIME_UNWRAP,
                        &request(&[0; 32], &ciphertext)
                    )
                    .status,
                status::DENIED
            );
        }
        for ct in [
            vec![0; 256],
            vec![0; 255],
            native_key.wrap_for_test(&[3; 24]),
        ] {
            let r = state.handle(&work, op::MAIL_SMIME_UNWRAP, &request(fingerprint, &ct));
            assert_eq!(r.status, status::MALFORMED);
            assert!(r.body.bytes().is_empty());
        }
        let unwrap = request(fingerprint, &native_key.wrap_for_test(&[5; 32]));
        let stored = state.stores["alice"]
            .list(&Access {
                persona: "work",
                recipient: &Recipient::Smime { publisher: [7; 32] },
            })
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].kind.shape(), Shape::Key);
        assert_eq!(
            state.handle(&work, op::RELEASE, &stored[0].id.0).status,
            status::DENIED
        );
        assert_eq!(
            state
                .handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice")))
                .status,
            status::OK
        );
        assert_eq!(
            state.handle(&work, op::MAIL_SMIME_UNWRAP, &unwrap).status,
            status::LOCKED
        );
        let unlock = body(|w| {
            w.str("alice");
            w.raw(&[1; 32]);
        });
        assert_eq!(
            state
                .handle(&Label::Vestibulum, op::V_UNLOCK, &unlock)
                .status,
            status::OK
        );
        assert_eq!(
            state.handle(&work, op::MAIL_SMIME_UNWRAP, &unwrap).status,
            status::OK
        );
        let forget = body(|w| {
            w.str("alice");
            w.str("work");
            w.raw(fingerprint);
        });
        assert_eq!(
            state.handle(&work, op::U_SMIME_FORGET, &forget).status,
            status::DENIED
        );
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_FORGET, &forget)
                .status,
            status::OK
        );
        assert_eq!(
            state.handle(&work, op::MAIL_SMIME_UNWRAP, &unwrap).status,
            status::DENIED
        );
        let personal = mail("alice", [7; 32], MAIL, None);
        let enroll = |persona| {
            body(|w| {
                w.str("alice");
                w.str(persona);
                w.str("Fixture");
                w.bytes(fixture);
            })
        };
        for persona in ["", "work"] {
            assert_eq!(
                state
                    .handle(&Label::TrustedUi, op::U_SMIME_IMPORT, &enroll(persona))
                    .status,
                status::OK
            );
        }
        // The ordinary persona erasure path removes the Work key only.
        let destroy = body(|w| {
            w.str("alice");
            w.str("work");
        });
        assert_eq!(
            state
                .handle(
                    &Label::Service("atrium-volumes".into()),
                    volume_op::PERSONA_DESTROY,
                    &destroy
                )
                .status,
            status::OK
        );
        assert_eq!(
            state.handle(&work, op::MAIL_SMIME_UNWRAP, &unwrap).status,
            status::DENIED
        );
        assert_eq!(
            state
                .handle(&personal, op::MAIL_SMIME_UNWRAP, &unwrap)
                .status,
            status::OK
        );
    }
    #[test]
    fn paired_provisioning_lists_only_public_certificates_for_the_calling_persona() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::new(dir.path().into()).unwrap();
        state.mail_sign_ins.mail_publisher = Some([7; 32]);
        let create = body(|w| {
            w.str("alice");
            w.raw(&[1; 32]);
        });
        assert_eq!(
            state
                .handle(&Label::Vestibulum, op::V_CREATE, &create)
                .status,
            status::OK
        );
        let work = mail("alice", [7; 32], MAIL, Some("work"));
        let personal = mail("alice", [7; 32], MAIL, None);
        let leaf = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../mail/mail-smime/tests/data/leaf.der"
        ));
        let ca = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../mail/mail-smime/tests/data/ca.der"
        ));
        let pkcs8 = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../mail/mail-smime/tests/data/leaf.p8"
        ));
        let provision = |cert| {
            body(|w| {
                w.str("alice");
                w.str("work");
                w.str("Mail test certificate");
                w.bytes(cert);
                w.bytes(pkcs8);
            })
        };
        assert_eq!(
            state
                .handle(&work, op::U_SMIME_PROVISION, &provision(leaf))
                .status,
            status::DENIED
        );
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_PROVISION, &provision(ca))
                .status,
            status::MALFORMED
        );
        let bare = body(|w| {
            w.str("alice");
            w.str("work");
            w.str("Existing bare key");
            w.bytes(pkcs8);
        });
        let bare = state.handle(&Label::TrustedUi, op::U_SMIME_IMPORT, &bare);
        assert_eq!(bare.status, status::OK);
        assert_eq!(
            state.handle(&work, op::MAIL_SMIME_CERTS, &[]).body.bytes(),
            [0; 4]
        );
        let added = state.handle(&Label::TrustedUi, op::U_SMIME_PROVISION, &provision(leaf));
        assert_eq!(added.status, status::OK);
        let hash = added.body.bytes();
        assert_eq!(hash, bare.body.bytes());
        let listed = state.handle(&work, op::MAIL_SMIME_CERTS, &[]);
        assert_eq!(listed.status, status::OK);
        assert_eq!(
            listed.body.bytes(),
            body(|w| {
                w.u32(1);
                w.bytes(leaf);
            })
        );
        assert_eq!(
            state
                .handle(&personal, op::MAIL_SMIME_CERTS, &[])
                .body
                .bytes(),
            [0, 0, 0, 0]
        );
        assert_eq!(
            state
                .handle(
                    &mail("alice", [8; 32], MAIL, Some("work")),
                    op::MAIL_SMIME_CERTS,
                    &[]
                )
                .status,
            status::DENIED
        );
        assert_eq!(
            state
                .handle(&work, op::MAIL_SMIME_CERTS, b"personal")
                .status,
            status::MALFORMED
        );
        let forget = body(|w| {
            w.str("alice");
            w.str("work");
            w.raw(hash);
        });
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_FORGET, &forget)
                .status,
            status::OK
        );
        assert_eq!(
            state.handle(&work, op::MAIL_SMIME_CERTS, &[]).body.bytes(),
            [0, 0, 0, 0]
        );
    }
}
