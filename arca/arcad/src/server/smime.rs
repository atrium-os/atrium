//! Arca-backed RSA-OAEP CMS key transport. Mail identifies a certificate
//! key by its public SPKI hash. Arca never accepts a persona from Mail.

use super::*;

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
    pub(super) fn smime_import(&mut self, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let mut r = Reader::new(body);
        let (Ok(user), Ok(persona), Ok(label), Ok(der)) = (
            r.str(caps::USER),
            r.str(caps::PERSONA),
            r.str(caps::ACCOUNT),
            r.bytes(caps::VALUE),
        ) else {
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
        if tri!(key_id(store, &access, &fingerprint)).is_some() {
            return reply(status::EXISTS);
        }
        let saved = tri!(store.add(NewItem {
            persona,
            recipient,
            class: Class::WhileUnlocked,
            kind: Kind::SmimeRsaKey,
            provenance: Provenance::Migrated,
            account: label,
            scope: &fingerprint,
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
        .find(|m| m.kind == Kind::SmimeRsaKey && m.scope == fingerprint)
        .map(|m| m.id))
}

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
}
