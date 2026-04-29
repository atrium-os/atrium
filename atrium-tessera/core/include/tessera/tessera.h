/*
 * tessera/tessera.h — umbrella header for tessera-core.
 *
 * Includes every public header so consumers can `#include <tessera/tessera.h>`
 * and pull the full API.
 */

#ifndef TESSERA_TESSERA_H_
#define TESSERA_TESSERA_H_

#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/hash.h"
#include "tessera/crc.h"
#include "tessera/codec.h"
#include "tessera/cdc.h"
#include "tessera/btree.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"
#include "tessera/journal.h"
#include "tessera/extent.h"
#include "tessera/gc.h"

#define TESSERA_VERSION_MAJOR  1
#define TESSERA_VERSION_MINOR  0

#endif /* TESSERA_TESSERA_H_ */
