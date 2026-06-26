/* tessera-core: error-string mapping. */

#include "tessera/error.h"

const char *
tessera_strerror(tessera_errno_t e)
{
	switch (e) {
	case TESSERA_OK:           return "ok";
	case TESSERA_EINVAL:       return "invalid argument";
	case TESSERA_ENOMEM:       return "out of memory";
	case TESSERA_ENOTIMPL:     return "not implemented";
	case TESSERA_EIO:          return "I/O error";
	case TESSERA_ENOSPC:       return "no space left";
	case TESSERA_ENOENT:       return "not found";
	case TESSERA_EEXIST:       return "already exists";
	case TESSERA_EBADMAGIC:    return "magic number mismatch";
	case TESSERA_EBADCRC:      return "CRC mismatch";
	case TESSERA_EBADHASH:     return "content hash mismatch";
	case TESSERA_EBADVERSION:  return "unsupported version";
	case TESSERA_EINCOMPAT:    return "unrecognized incompat feature";
	case TESSERA_ETOOBIG:      return "too big";
	case TESSERA_ECORRUPT:     return "on-disk inconsistency";
	case TESSERA_EJOURNAL:     return "journal replay failed";
	case TESSERA_EBUSY:        return "resource busy";
	case TESSERA_ELOOP:        return "directory cycle detected";
	case TESSERA_ENOTDIR:      return "not a directory";
	case TESSERA_EISDIR:       return "is a directory";
	case TESSERA_ENOTEMPTY:    return "directory not empty";
	case TESSERA_EPERM:        return "operation not permitted";
	case TESSERA_EXDEV:        return "cross-device";
	case TESSERA_ERANGE:       return "out of range";
	case TESSERA_EDQUOT:       return "quota exceeded";
	}
	return "unknown error";
}
