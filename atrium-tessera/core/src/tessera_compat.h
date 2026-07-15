/*
 * tessera_compat.h — kernel/userspace include + allocator shim.
 *
 * tessera-core sources are compiled in two contexts:
 *   - userspace tools (mkfs-tessera, tessera-debug, tests): glibc/musl
 *     style stdlib.h + string.h.
 *   - tessera_fs.ko (FreeBSD kernel): -nostdinc, no stdlib.h, kernel
 *     malloc with type tags + flags.
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
