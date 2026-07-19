/*
 * tessera_compat.h — kernel/userspace include + allocator shim.
 *
 * tessera-core sources are compiled in three contexts:
 *   - userspace tools (mkfs-tessera, tessera-debug, tests): glibc/musl
 *     style stdlib.h + string.h.
 *   - tessera_fs.ko (FreeBSD kernel): -nostdinc, no stdlib.h, kernel
 *     malloc with type tags + flags.
 *   - the FreeBSD loader (stand/libsa, -DTESSERA_STAND): libsa's own
 *     malloc/free/printf + string.h. Read-only reader subset only.
 *
 * This header papers over the difference. Internal-only — not part of
 * the public API surface and never included from include/tessera/.
 */

#ifndef TESSERA_COMPAT_H_
#define TESSERA_COMPAT_H_

#ifdef TESSERA_KERNEL
#  include <sys/types.h>
#  include <sys/param.h>
#  include <sys/systm.h>      /* memcpy, memcmp, memset */
#  include <sys/libkern.h>
#  include <sys/malloc.h>
   MALLOC_DECLARE(M_TESSERA);
#  define tessera_malloc(n)        malloc((n), M_TESSERA, M_WAITOK)
#  define tessera_zalloc(n)        malloc((n), M_TESSERA, M_WAITOK | M_ZERO)
#  define tessera_calloc(c, n)     malloc((c) * (n), M_TESSERA, M_WAITOK | M_ZERO)
#  define tessera_realloc(p, n)    realloc((p), (n), M_TESSERA, M_WAITOK)
#  define tessera_free(p)          free((p), M_TESSERA)
#  define tessera_debugf(...)      printf(__VA_ARGS__)
#elif defined(TESSERA_STAND)
   /* FreeBSD loader (libsa). string.h supplies memcpy/memset/memcmp;
    * stand.h supplies the allocator + printf. Read-only reader path only.
    *
    * libsa #define's malloc/calloc/free/realloc as function-like macros,
    * which collide with the block_io struct's `alloc`/`free` MEMBERS when
    * they're called as `io.free(...)` (dead code for the reader, but it
    * still compiles). Undo the macros and bind the tessera_* allocators
    * straight to the underlying Malloc/Calloc/Reallocf/Free. */
#  include <sys/types.h>
#  include <string.h>
#  include "stand.h"
#  undef malloc
#  undef calloc
#  undef free
#  undef realloc
#  undef reallocf
#  define tessera_malloc(n)        Malloc((n), NULL, 0)
#  define tessera_zalloc(n)        Calloc(1u, (n), NULL, 0)
#  define tessera_calloc(c, n)     Calloc((c), (n), NULL, 0)
#  define tessera_realloc(p, n)    Reallocf((p), (n), NULL, 0)
#  define tessera_free(p)          Free((p), NULL, 0)
#  define tessera_debugf(...)      printf(__VA_ARGS__)
   /* libsa has no qsort; the pack builder (never called by the reader)
    * references it. Declared here, provided by the loader glue. */
void tessera_stand_qsort(void *, size_t, size_t,
                         int (*)(const void *, const void *));
#  define qsort(base, n, sz, cmp)  tessera_stand_qsort((base), (n), (sz), (cmp))
#else
#  include <stdlib.h>
#  include <string.h>
#  include <stdio.h>
#  define tessera_debugf(...)      fprintf(stderr, __VA_ARGS__)
#  define tessera_malloc(n)        malloc(n)
#  define tessera_zalloc(n)        calloc(1u, (n))
#  define tessera_calloc(c, n)     calloc((c), (n))
#  define tessera_realloc(p, n)    realloc((p), (n))
#  define tessera_free(p)          free(p)
#endif

#endif /* TESSERA_COMPAT_H_ */
