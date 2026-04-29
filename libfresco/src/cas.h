/*
 * cas.h — internal CAS (content-addressed store) helpers.
 */

#ifndef _FRESCO_CAS_H_
#define _FRESCO_CAS_H_

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

/* Local dedup cache: small open-addressed table keyed on hash[0..8].
 * Entries with `present = false` are empty. On collision/full table,
 * we evict the entry at the probed slot — POC-quality, but stable
 * enough for typical scene-graph workloads where blobs cluster. */

#define FRESCO_CAS_CACHE_SLOTS  256

struct fresco_cas_entry {
    uint8_t  hash[32];
    bool     present;
};

struct fresco_cas_cache {
    struct fresco_cas_entry slots[FRESCO_CAS_CACHE_SLOTS];
};

void fresco_cas_cache_init  (struct fresco_cas_cache *c);
bool fresco_cas_cache_has   (struct fresco_cas_cache *c, const uint8_t hash[32]);
void fresco_cas_cache_insert(struct fresco_cas_cache *c, const uint8_t hash[32]);

#endif /* _FRESCO_CAS_H_ */
