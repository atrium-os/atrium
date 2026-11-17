//! Tessera volume keys (tessera-encryption.md §4.3): the kernel keyring.
//!
//! A `VolumeKey` item is used by the kernel and by nobody else. arcad opens
//! `/dev/tesserakeys` at start and is the only process that loads keys into
//! it; the key goes from the store's locked memory into the ioctl buffer and
//! is wiped there. No op returns it.
//!
//! The item: scope `key_id[16] | volume uuid[16] | group u64 LE`, value
//! `key[32] | high_water u64 LE`, account = the label Scrinium and Curia show.

use std::io;

/// The recipient every volume key is stored for. It is not a caller: the
/// volume ops look items up under it on behalf of [`SERVICES`], and no
/// service may take this name (`label::parse_services` drops it).
pub const RECIPIENT: &str = "tessera-keyring";

/// The services that may use the volume ops: dispensatord for sticks,
/// atrium-volumes (run by portcullisd at login) for key groups.
pub const SERVICES: [&str; 2] = ["dispensatord", "atrium-volumes"];

pub const SCOPE_LEN: usize = 40;
pub const VALUE_LEN: usize = 40;

/// What the kernel is told with a key (`struct tkey_load` minus the key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadInfo {
    pub key_id: [u8; 16],
    pub uuid: [u8; 16],
    pub group: u64,
    pub high_water: u64,
    pub owner_uid: u32,
    /// `user/persona`, cut to 31 bytes.
    pub owner_label: String,
}

/// The kernel keyring, or a stand-in for tests.
pub trait Keyring: Send {
    /// Load `key` for `info`'s scope. `Err(EEXIST)` if the id is loaded.
    fn load(&mut self, info: &LoadInfo, key: &[u8; 32]) -> Result<(), i32>;
    /// `Err(ENOENT)` if not loaded.
    fn drop_key(&mut self, key_id: &[u8; 16]) -> Result<(), i32>;
    /// The highest generation committed under the key since it was loaded.
    fn generation(&mut self, key_id: &[u8; 16]) -> Result<u64, i32>;
}

pub fn encode_scope(key_id: &[u8; 16], uuid: &[u8; 16], group: u64) -> [u8; SCOPE_LEN] {
    let mut s = [0u8; SCOPE_LEN];
    s[..16].copy_from_slice(key_id);
    s[16..32].copy_from_slice(uuid);
    s[32..].copy_from_slice(&group.to_le_bytes());
    s
}

/// `(key_id, uuid, group)`, or `None` for a scope that isn't a volume key's.
pub fn decode_scope(s: &[u8]) -> Option<([u8; 16], [u8; 16], u64)> {
    if s.len() != SCOPE_LEN {
        return None;
    }
    Some((s[..16].try_into().ok()?, s[16..32].try_into().ok()?, u64::from_le_bytes(s[32..].try_into().ok()?)))
}

pub fn owner_label(user: &str, persona: &str) -> String {
    let mut l = format!("{user}/{persona}");
    while l.len() > 31 {
        l.pop();
    }
    l
}

/// `/dev/tesserakeys` (atrium-tessera/core/include/tessera/keyring.h).
#[cfg(target_os = "freebsd")]
pub mod dev {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct TkeyLoad {
        key_id: [u8; 16],
        scope_uuid: [u8; 16],
        scope_group: u64,
        key: [u8; 32],
        high_water: u64,
        owner_uid: u32,
        flags: u32,
        owner_label: [u8; 32],
    }
    #[repr(C)]
    struct TkeyId {
        key_id: [u8; 16],
    }
    #[repr(C)]
    struct TkeyGen {
        key_id: [u8; 16],
        generation: u64,
    }
    const _: () = assert!(std::mem::size_of::<TkeyLoad>() == 120);
    const _: () = assert!(std::mem::size_of::<TkeyGen>() == 24);

    // _IOW('K', 1, struct tkey_load), _IOW('K', 2, struct tkey_id),
    // _IOWR('K', 3, struct tkey_gen) (sys/ioccom.h).
    const fn ioc(inout: u64, n: u64, len: usize) -> libc::c_ulong {
        (inout | ((len as u64 & 0x1fff) << 16) | ((b'K' as u64) << 8) | n) as libc::c_ulong
    }
    const IOC_IN: u64 = 0x8000_0000;
    const IOC_INOUT: u64 = 0xc000_0000;
    const TKEY_LOAD: libc::c_ulong = ioc(IOC_IN, 1, std::mem::size_of::<TkeyLoad>());
    const TKEY_DROP: libc::c_ulong = ioc(IOC_IN, 2, std::mem::size_of::<TkeyId>());
    const TKEY_GEN: libc::c_ulong = ioc(IOC_INOUT, 3, std::mem::size_of::<TkeyGen>());

    pub const PATH: &str = "/dev/tesserakeys";

    pub struct Device(File);

    impl Device {
        /// Open the keyring for writing. Done once, before arcad serves.
        pub fn open(path: &str) -> io::Result<Device> {
            Ok(Device(std::fs::OpenOptions::new().read(true).write(true).open(path)?))
        }
        fn ioctl<T>(&self, cmd: libc::c_ulong, arg: &mut T) -> Result<(), i32> {
            // SAFETY: arg is a live, correctly sized #[repr(C)] struct that
            // matches the command's encoded length.
            let r = unsafe { libc::ioctl(self.0.as_raw_fd(), cmd, arg as *mut T) };
            if r == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    impl Keyring for Device {
        fn load(&mut self, info: &LoadInfo, key: &[u8; 32]) -> Result<(), i32> {
            let mut l = TkeyLoad {
                key_id: info.key_id,
                scope_uuid: info.uuid,
                scope_group: info.group,
                key: *key,
                high_water: info.high_water,
                owner_uid: info.owner_uid,
                flags: 0,
                owner_label: [0; 32],
            };
            let lb = info.owner_label.as_bytes();
            l.owner_label[..lb.len().min(31)].copy_from_slice(&lb[..lb.len().min(31)]);
            let r = self.ioctl(TKEY_LOAD, &mut l);
            // SAFETY: plain bytes; volatile so the wipe is not elided.
            for b in l.key.iter_mut() {
                unsafe { std::ptr::write_volatile(b, 0) };
            }
            r
        }
        fn drop_key(&mut self, key_id: &[u8; 16]) -> Result<(), i32> {
            self.ioctl(TKEY_DROP, &mut TkeyId { key_id: *key_id })
        }
        fn generation(&mut self, key_id: &[u8; 16]) -> Result<u64, i32> {
            let mut g = TkeyGen { key_id: *key_id, generation: 0 };
            self.ioctl(TKEY_GEN, &mut g)?;
            Ok(g.generation)
        }
    }
}

/// A keyring in memory, for tests: what was loaded, and generations to
/// report. Keeps the keys so a test can check they match the item.
#[derive(Default)]
pub struct MemKeyring {
    pub loaded: Vec<(LoadInfo, [u8; 32])>,
    pub generations: Vec<([u8; 16], u64)>,
}

impl Keyring for MemKeyring {
    fn load(&mut self, info: &LoadInfo, key: &[u8; 32]) -> Result<(), i32> {
        if self.loaded.iter().any(|(i, _)| i.key_id == info.key_id) {
            return Err(libc::EEXIST);
        }
        self.loaded.push((info.clone(), *key));
        Ok(())
    }
    fn drop_key(&mut self, key_id: &[u8; 16]) -> Result<(), i32> {
        let n = self.loaded.len();
        self.loaded.retain(|(i, _)| &i.key_id != key_id);
        if self.loaded.len() == n {
            Err(libc::ENOENT)
        } else {
            Ok(())
        }
    }
    fn generation(&mut self, key_id: &[u8; 16]) -> Result<u64, i32> {
        if !self.loaded.iter().any(|(i, _)| &i.key_id == key_id) {
            return Err(libc::ENOENT);
        }
        Ok(self.generations.iter().find(|(k, _)| k == key_id).map(|(_, g)| *g).unwrap_or(0))
    }
}

/// Shared in tests: the state owns a boxed keyring, the test keeps a handle.
impl<K: Keyring> Keyring for std::sync::Arc<std::sync::Mutex<K>> {
    fn load(&mut self, info: &LoadInfo, key: &[u8; 32]) -> Result<(), i32> {
        self.lock().map_err(|_| libc::EIO)?.load(info, key)
    }
    fn drop_key(&mut self, key_id: &[u8; 16]) -> Result<(), i32> {
        self.lock().map_err(|_| libc::EIO)?.drop_key(key_id)
    }
    fn generation(&mut self, key_id: &[u8; 16]) -> Result<u64, i32> {
        self.lock().map_err(|_| libc::EIO)?.generation(key_id)
    }
}

#[allow(dead_code)]
fn _io(_: io::Error) {}
