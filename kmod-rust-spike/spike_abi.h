/*
 * spike_abi.h -- ioctl ABI shared by the Rust kernel module (rspike.ko,
 * /dev/rspike), its C twin (cspike.ko, /dev/cspike), the benchmark and the
 * syzkaller description.  Both modules implement exactly this contract, so
 * the same test drives either and their results must agree.
 *
 * The operations are the four things Atrium kernel code does with user input:
 *   NOP    pure dispatch (the latency floor of an ioctl into the module)
 *   SET    copyin a user buffer into a malloc(9)ed blob, swap it in under sx
 *   GET    copyout the blob under a shared sx lock
 *   PARSE  copyin a record stream and decode it (the shape of an on-disk
 *          decoder such as Tessera's): bounds, nesting, rejection
 *   STATS  counters under a mutex
 * read(2)/write(2) are a 4 KiB byte FIFO through uiomove(9).
 *
 * PARSE record format, little-endian, repeated until the buffer ends:
 *   u8 type   1..0xfe: a leaf; its value bytes are summed into `sum`
 *             0xff:    a container; its value is itself a record stream,
 *                      nested at most SPIKE_MAX_DEPTH deep
 *             0:       invalid
 *   u8 flags  must be 0
 *   u16 len   value length; the value must fit in what remains
 * `count` is the number of leaf records.  Any violation is EINVAL and leaves
 * count/sum unchanged.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */
#ifndef SPIKE_ABI_H
#define SPIKE_ABI_H

#include <sys/types.h>
#include <sys/ioccom.h>

#define	SPIKE_MAX_BLOB	65536
#define	SPIKE_MAX_DEPTH	4
#define	SPIKE_FIFO_SIZE	4096

struct spike_buf {
	uint64_t	ptr;	/* user address */
	uint32_t	len;	/* in: buffer length; out (GET): blob length */
	uint32_t	flags;	/* must be 0 */
};

struct spike_parse {
	uint64_t	ptr;
	uint32_t	len;
	uint32_t	count;	/* out */
	uint64_t	sum;	/* out */
};

struct spike_stats {
	uint64_t	nops;
	uint64_t	sets;
	uint64_t	gets;
	uint64_t	parses;
	uint64_t	parse_errors;
	uint64_t	bytes_in;
	uint64_t	bytes_out;
};

#define	SPIKE_IOC_NOP	_IO('S', 0)
#define	SPIKE_IOC_SET	_IOW('S', 1, struct spike_buf)
#define	SPIKE_IOC_GET	_IOWR('S', 2, struct spike_buf)
#define	SPIKE_IOC_PARSE	_IOWR('S', 3, struct spike_parse)
#define	SPIKE_IOC_STATS	_IOR('S', 4, struct spike_stats)

#endif
