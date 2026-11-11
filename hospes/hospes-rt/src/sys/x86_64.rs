//! x86_64 trap. Linux returns -errno in rax; FreeBSD sets the carry flag and returns errno.
//! FreeBSD also returns a second value in rdx, so rdx is always clobbered.
use core::arch::asm;

#[inline(always)]
pub unsafe fn sc(n: usize, a: [usize; 6]) -> (usize, bool) {
    let ret: usize;
    let carry: u8;
    asm!(
        "syscall",
        "setc {c}",
        c = out(reg_byte) carry,
        inlateout("rax") n => ret,
        inlateout("rdi") a[0] => _,
        inlateout("rsi") a[1] => _,
        inlateout("rdx") a[2] => _,
        inlateout("r10") a[3] => _,
        inlateout("r8") a[4] => _,
        inlateout("r9") a[5] => _,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    (ret, carry != 0)
}

/// The thread pointer (`%fs` base, read through the TCB self-pointer at `%fs:0`).
#[inline(always)]
pub fn tp() -> usize {
    let v: usize;
    unsafe { asm!("mov {}, fs:0", out(reg) v, options(nostack, readonly, preserves_flags)) };
    v
}
