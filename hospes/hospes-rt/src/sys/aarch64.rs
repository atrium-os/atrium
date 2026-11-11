//! aarch64 trap: FreeBSD puts the number in x8 and flags errors with the carry flag; Linux
//! uses x8 and returns -errno. x1 is FreeBSD's second return value.
use core::arch::asm;

#[inline(always)]
pub unsafe fn sc(n: usize, a: [usize; 6]) -> (usize, bool) {
    let ret: usize;
    let carry: usize;
    asm!(
        "svc #0",
        "cset {c}, cs",
        c = lateout(reg) carry,
        in("x8") n,
        inlateout("x0") a[0] => ret,
        inlateout("x1") a[1] => _,
        inlateout("x2") a[2] => _,
        inlateout("x3") a[3] => _,
        inlateout("x4") a[4] => _,
        inlateout("x5") a[5] => _,
        options(nostack)
    );
    (ret, carry != 0)
}

#[inline(always)]
pub fn tp() -> usize {
    let v: usize;
    unsafe { asm!("mrs {}, tpidr_el0", out(reg) v, options(nostack, nomem, preserves_flags)) };
    v
}

#[inline(always)]
pub unsafe fn set_tp_reg(v: usize) {
    asm!("msr tpidr_el0, {}", in(reg) v, options(nostack, nomem, preserves_flags));
}
