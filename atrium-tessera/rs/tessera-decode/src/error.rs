//! Return codes, identical to `tessera_errno_t` (core/include/tessera/error.h).

pub type Error = i32;

pub const OK: Error = 0;
pub const EINVAL: Error = -1;
pub const ENOMEM: Error = -2;
pub const ENOENT: Error = -6;
pub const EBADMAGIC: Error = -8;
pub const EBADCRC: Error = -9;
pub const ECORRUPT: Error = -14;

pub type Result<T> = core::result::Result<T, Error>;
