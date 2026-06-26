/*
 * tessera/error.h — error codes returned by tessera-core functions.
 *
 * All public functions return 0 on success and a negative tessera_errno_t
 * on failure. Codes are stable across versions; new codes are added at
 * the end of the enum.
 */

#ifndef TESSERA_ERROR_H_
#define TESSERA_ERROR_H_

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
	TESSERA_OK              =   0,

	TESSERA_EINVAL          =  -1,	/* invalid argument */
	TESSERA_ENOMEM          =  -2,	/* allocation failure */
	TESSERA_ENOTIMPL        =  -3,	/* not yet implemented (phase 0) */
	TESSERA_EIO             =  -4,	/* underlying I/O error */
	TESSERA_ENOSPC          =  -5,	/* no space left on volume */
	TESSERA_ENOENT          =  -6,	/* not found */
	TESSERA_EEXIST          =  -7,	/* already exists */
	TESSERA_EBADMAGIC       =  -8,	/* magic number mismatch */
	TESSERA_EBADCRC         =  -9,	/* CRC mismatch */
	TESSERA_EBADHASH        = -10,	/* content hash mismatch */
	TESSERA_EBADVERSION     = -11,	/* unsupported version */
	TESSERA_EINCOMPAT       = -12,	/* unrecognized incompat feature */
	TESSERA_ETOOBIG         = -13,	/* exceeds spec limit */
	TESSERA_ECORRUPT        = -14,	/* on-disk inconsistency */
	TESSERA_EJOURNAL        = -15,	/* journal replay failed */
	TESSERA_EBUSY           = -16,	/* resource busy */
	TESSERA_ELOOP           = -17,	/* directory cycle detected */
	TESSERA_ENOTDIR         = -18,
	TESSERA_EISDIR          = -19,
	TESSERA_ENOTEMPTY       = -20,
	TESSERA_EPERM           = -21,	/* operation not permitted (chflags etc.) */
	TESSERA_EXDEV           = -22,	/* cross-device operation */
	TESSERA_ERANGE          = -23,	/* offset/length out of range */
	TESSERA_EDQUOT          = -24,	/* quota domain limit exceeded */
} tessera_errno_t;

/* Human-readable description of a code (for logs, never NULL). */
const char *tessera_strerror(tessera_errno_t e);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_ERROR_H_ */
