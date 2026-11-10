//! Little-endian field reads at byte offsets. Every read is bounds-checked and
//! returns `None` past the end; none can panic.

#[inline]
pub fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes(s.try_into().ok()?))
}

#[inline]
pub fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}

#[inline]
pub fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(s.try_into().ok()?))
}

#[inline]
pub fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

/// `b[off..off + len]`, or `None` if any part of it is outside `b` or the
/// end overflows.
#[inline]
pub fn range(b: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    b.get(off..off.checked_add(len)?)
}

/// A u64 from disk as a usize offset. `None` if it does not fit (32-bit hosts).
#[inline]
pub fn usize_of(v: u64) -> Option<usize> {
    usize::try_from(v).ok()
}
