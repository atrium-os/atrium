use super::*;
use sigillum_ccid::fake::{innait_desc, Fake};
use std::sync::Mutex;
use std::vec;

/// The PC/SC state is one per process: tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

const NAME: &str = "Precision InnaIT Key [CCID Interface] 00 00";

fn setup(card: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static, present: bool) {
    let mut f = Fake::new(card);
    f.present = present;
    let r = Reader::new(f, innait_desc()).unwrap();
    install(Setup { card: Box::new(r), name: NAME.into(), sleep_ms: |_| {} });
}

fn ctx() -> Context {
    let mut c = 0;
    assert_eq!(unsafe { SCardEstablishContext(2, core::ptr::null(), core::ptr::null(), &mut c) }, SCARD_S_SUCCESS);
    c
}

fn name() -> std::ffi::CString {
    std::ffi::CString::new(NAME).unwrap()
}

#[test]
fn list_connect_transmit_disconnect() {
    let _g = SERIAL.lock().unwrap();
    setup(|a: &[u8]| if a[1] == 0xA4 { vec![0x90, 0x00] } else { vec![0x6D, 0x00] }, true);
    let c = ctx();
    unsafe {
        // Size, then the multi-string.
        let mut n: Dword = 0;
        assert_eq!(SCardListReaders(c, core::ptr::null(), core::ptr::null_mut(), &mut n), SCARD_S_SUCCESS);
        assert_eq!(n as usize, NAME.len() + 2);
        let mut buf = vec![0u8; n as usize];
        assert_eq!(SCardListReaders(c, core::ptr::null(), buf.as_mut_ptr() as *mut c_char, &mut n), SCARD_S_SUCCESS);
        assert!(buf.starts_with(NAME.as_bytes()) && buf.ends_with(&[0, 0]));
        // Too small, then auto-allocated.
        let mut small: Dword = 4;
        assert_eq!(SCardListReaders(c, core::ptr::null(), buf.as_mut_ptr() as *mut c_char, &mut small), SCARD_E_INSUFFICIENT_BUFFER);
        let mut auto: *mut u8 = core::ptr::null_mut();
        let mut al = SCARD_AUTOALLOCATE;
        assert_eq!(SCardListReaders(c, core::ptr::null(), &mut auto as *mut *mut u8 as *mut c_char, &mut al), SCARD_S_SUCCESS);
        assert_eq!(core::slice::from_raw_parts(auto, al as usize), &buf[..]);
        assert_eq!(SCardFreeMemory(c, auto as *const c_void), SCARD_S_SUCCESS);

        let (mut h, mut p) = (0, 0);
        assert_eq!(SCardConnect(c, name().as_ptr(), 2, SCARD_PROTOCOL_T0 | SCARD_PROTOCOL_T1, &mut h, &mut p), SCARD_S_SUCCESS);
        assert_eq!(p, SCARD_PROTOCOL_T1);
        assert_eq!(SCardBeginTransaction(h), SCARD_S_SUCCESS);
        let pci = IoRequest { protocol: SCARD_PROTOCOL_T1, pci_length: 16 };
        let apdu = [0u8, 0xA4, 4, 0, 1, 0xA0];
        let mut out = [0u8; 258];
        let mut ol: Dword = out.len() as Dword;
        assert_eq!(SCardTransmit(h, &pci, apdu.as_ptr(), 6, core::ptr::null_mut(), out.as_mut_ptr(), &mut ol), SCARD_S_SUCCESS);
        assert_eq!(&out[..ol as usize], &[0x90, 0x00]);
        // Wrong protocol in the PCI; a buffer too small for the response.
        let t0 = IoRequest { protocol: SCARD_PROTOCOL_T0, pci_length: 16 };
        assert_eq!(SCardTransmit(h, &t0, apdu.as_ptr(), 6, core::ptr::null_mut(), out.as_mut_ptr(), &mut ol), SCARD_E_PROTO_MISMATCH);
        let mut one: Dword = 1;
        assert_eq!(SCardTransmit(h, &pci, apdu.as_ptr(), 6, core::ptr::null_mut(), out.as_mut_ptr(), &mut one), SCARD_E_INSUFFICIENT_BUFFER);
        assert_eq!(one, 2);

        let (mut st, mut proto) = (0, 0);
        let mut atr = [0u8; 33];
        let mut atr_len: Dword = 33;
        assert_eq!(
            SCardStatus(h, core::ptr::null_mut(), core::ptr::null_mut(), &mut st, &mut proto, atr.as_mut_ptr(), &mut atr_len),
            SCARD_S_SUCCESS
        );
        assert_eq!((st & SCARD_PRESENT, proto, atr_len), (SCARD_PRESENT, SCARD_PROTOCOL_T1, 19));
        assert_eq!(&atr[..2], &[0x3B, 0x8E]);

        assert_eq!(SCardEndTransaction(h, SCARD_LEAVE_CARD), SCARD_S_SUCCESS);
        assert_eq!(SCardDisconnect(h, SCARD_RESET_CARD), SCARD_S_SUCCESS);
        assert_eq!(SCardTransmit(h, &pci, apdu.as_ptr(), 6, core::ptr::null_mut(), out.as_mut_ptr(), &mut ol), SCARD_E_INVALID_HANDLE);
        assert_eq!(SCardReleaseContext(c), SCARD_S_SUCCESS);
        assert_eq!(SCardReleaseContext(c), SCARD_E_INVALID_HANDLE);
    }
    uninstall();
}

#[test]
fn unknown_reader_and_no_card() {
    let _g = SERIAL.lock().unwrap();
    setup(|_: &[u8]| vec![0x90, 0x00], false);
    let c = ctx();
    unsafe {
        let (mut h, mut p) = (0, 0);
        let other = std::ffi::CString::new("Some Other Reader 00 00").unwrap();
        assert_eq!(SCardConnect(c, other.as_ptr(), 2, 3, &mut h, &mut p), SCARD_E_UNKNOWN_READER);
        assert_eq!(SCardConnect(c, name().as_ptr(), 2, 3, &mut h, &mut p), SCARD_E_NO_SMARTCARD);
        assert_eq!(SCardConnect(c + 99, name().as_ptr(), 2, 3, &mut h, &mut p), SCARD_E_INVALID_HANDLE);
    }
    uninstall();
    let mut c2 = 0;
    assert_eq!(unsafe { SCardEstablishContext(2, core::ptr::null(), core::ptr::null(), &mut c2) }, SCARD_E_NO_READERS_AVAILABLE);
}

#[test]
fn status_change() {
    let _g = SERIAL.lock().unwrap();
    setup(|_: &[u8]| vec![0x90, 0x00], true);
    let c = ctx();
    let n = name();
    let pnp = std::ffi::CString::new("\\\\?PnP?\\Notification").unwrap();
    let mut rs = [
        ReaderState { reader: n.as_ptr(), user: core::ptr::null_mut(), current: 0, event: 0, atr_len: 0, atr: [0; 33] },
        ReaderState { reader: pnp.as_ptr(), user: core::ptr::null_mut(), current: 0, event: 0, atr_len: 0, atr: [0; 33] },
    ];
    unsafe {
        // Unaware: the present card is a change, with its ATR.
        assert_eq!(SCardGetStatusChange(c, 0, rs.as_mut_ptr(), 2), SCARD_S_SUCCESS);
        assert_ne!(rs[0].event & SCARD_STATE_CHANGED, 0);
        assert_ne!(rs[0].event & SCARD_STATE_PRESENT, 0);
        assert_eq!(rs[0].atr_len, 19);
        // Up to date: nothing changes, so the wait times out.
        rs[0].current = rs[0].event & !SCARD_STATE_CHANGED;
        rs[1].current = rs[1].event;
        assert_eq!(SCardGetStatusChange(c, 600, rs.as_mut_ptr(), 2), SCARD_E_TIMEOUT);
        assert_eq!(rs[0].event & SCARD_STATE_CHANGED, 0);
    }
    uninstall();
}

#[test]
fn every_import_of_the_module_resolves() {
    for n in [
        "SCardBeginTransaction",
        "SCardConnect",
        "SCardDisconnect",
        "SCardEndTransaction",
        "SCardEstablishContext",
        "SCardFreeMemory",
        "SCardGetStatusChange",
        "SCardListReaders",
        "SCardReconnect",
        "SCardReleaseContext",
        "SCardStatus",
        "SCardTransmit",
    ] {
        assert!(resolve(n).is_some(), "{n}");
    }
    assert!(resolve("SCardControl").is_none());
}
