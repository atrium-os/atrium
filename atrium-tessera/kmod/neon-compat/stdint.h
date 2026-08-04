/*
 * neon-compat/stdint.h — satisfy <arm_neon.h>'s `#include <stdint.h>` in a
 * -nostdinc kernel build, portably across host and cross toolchains.
 *
 * b3_blake3_neon.c is the one translation unit that needs the compiler's
 * arm_neon.h intrinsics header (#94). Getting at it means putting the
 * compiler's resource directory back on the include path, and arm_neon.h
 * opens with `#include <stdint.h>`. What that resolves to differs by
 * toolchain, and BOTH ways were wrong:
 *
 *   - FreeBSD's in-tree clang 19: resource dir has arm_neon.h but no
 *     stdint.h, so the include is simply not found -> fatal error.
 *   - Homebrew clang 21 (macOS cross-build): resource dir has both, and its
 *     stdint.h redefines int_fast16_t as int16_t where FreeBSD's headers
 *     have __int_fast16_t as int -> typedef redefinition, -Werror, dead.
 *
 * The old rule dropped -nostdinc entirely (the pattern in FreeBSD's own
 * sys/modules/armv8crypto/Makefile). That only ever worked because the
 * compiler was FreeBSD's own; it breaks the moment you cross-build.
 *
 * So: this directory goes on the include path AHEAD of the resource dir.
 * arm_neon.h still comes from the resource dir (nothing else provides it),
 * but its <stdint.h> lands here and gets the kernel's own fixed-width types,
 * which is what the rest of the module is already compiled against.
 */
#ifndef _TESSERA_NEON_COMPAT_STDINT_H_
#define _TESSERA_NEON_COMPAT_STDINT_H_

#include <sys/stdint.h>

#endif /* _TESSERA_NEON_COMPAT_STDINT_H_ */
