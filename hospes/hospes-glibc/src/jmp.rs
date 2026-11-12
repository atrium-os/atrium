//! `_setjmp` / `longjmp`. Only the library itself fills and reads its `jmp_buf`s, so the
//! layout inside glibc's buffer (200 bytes on x86_64, 312 on aarch64) is ours: the
//! callee-saved registers, the stack pointer and the return address. No signal mask is
//! saved or restored (the library's handlers never run: §6.2), which is what `_setjmp`
//! promises and `longjmp` after it needs.

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".text",
    ".globl hospes_setjmp",
    ".type hospes_setjmp,@function",
    "hospes_setjmp:",
    "mov [rdi], rbx",
    "mov [rdi + 8], rbp",
    "mov [rdi + 16], r12",
    "mov [rdi + 24], r13",
    "mov [rdi + 32], r14",
    "mov [rdi + 40], r15",
    "lea rdx, [rsp + 8]",
    "mov [rdi + 48], rdx",
    "mov rdx, [rsp]",
    "mov [rdi + 56], rdx",
    "xor eax, eax",
    "ret",
    ".size hospes_setjmp, . - hospes_setjmp",
    ".globl hospes_longjmp",
    ".type hospes_longjmp,@function",
    "hospes_longjmp:",
    "mov eax, esi",
    "test eax, eax",
    "jnz 1f",
    "inc eax",
    "1:",
    "mov rbx, [rdi]",
    "mov rbp, [rdi + 8]",
    "mov r12, [rdi + 16]",
    "mov r13, [rdi + 24]",
    "mov r14, [rdi + 32]",
    "mov r15, [rdi + 40]",
    "mov rsp, [rdi + 48]",
    "jmp qword ptr [rdi + 56]",
    ".size hospes_longjmp, . - hospes_longjmp",
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".text",
    ".globl hospes_setjmp",
    ".type hospes_setjmp,@function",
    "hospes_setjmp:",
    "stp x19, x20, [x0, #0]",
    "stp x21, x22, [x0, #16]",
    "stp x23, x24, [x0, #32]",
    "stp x25, x26, [x0, #48]",
    "stp x27, x28, [x0, #64]",
    "stp x29, x30, [x0, #80]",
    "mov x2, sp",
    "str x2, [x0, #96]",
    "stp d8, d9, [x0, #112]",
    "stp d10, d11, [x0, #128]",
    "stp d12, d13, [x0, #144]",
    "stp d14, d15, [x0, #160]",
    "mov w0, #0",
    "ret",
    ".size hospes_setjmp, . - hospes_setjmp",
    ".globl hospes_longjmp",
    ".type hospes_longjmp,@function",
    "hospes_longjmp:",
    "ldp x19, x20, [x0, #0]",
    "ldp x21, x22, [x0, #16]",
    "ldp x23, x24, [x0, #32]",
    "ldp x25, x26, [x0, #48]",
    "ldp x27, x28, [x0, #64]",
    "ldp x29, x30, [x0, #80]",
    "ldr x2, [x0, #96]",
    "mov sp, x2",
    "ldp d8, d9, [x0, #112]",
    "ldp d10, d11, [x0, #128]",
    "ldp d12, d13, [x0, #144]",
    "ldp d14, d15, [x0, #160]",
    "cmp w1, #0",
    "csinc w0, w1, wzr, ne",
    "br x30",
    ".size hospes_longjmp, . - hospes_longjmp",
);

unsafe extern "C" {
    pub fn hospes_setjmp(buf: *mut u8) -> i32;
    pub fn hospes_longjmp(buf: *mut u8, val: i32) -> !;
}


#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    unsafe fn jump(buf: *mut u8, v: i32) -> ! {
        hospes_longjmp(buf, v)
    }

    #[test]
    fn round_trip() {
        let mut buf = [0u8; 312];
        let p = buf.as_mut_ptr();
        let mut hits = 0u32;
        let hits_p = core::ptr::addr_of_mut!(hits);
        unsafe {
            let r = core::ptr::read_volatile(&hospes_setjmp(p));
            let n = core::ptr::read_volatile(hits_p) + 1;
            core::ptr::write_volatile(hits_p, n);
            match n {
                1 => {
                    assert_eq!(r, 0);
                    jump(p, 0) // 0 comes back as 1
                }
                2 => {
                    assert_eq!(r, 1);
                    jump(p, 7)
                }
                _ => assert_eq!(r, 7),
            }
        }
        assert_eq!(hits, 3);
    }
}
