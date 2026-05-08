/*
 * atrium_trace_kmod.c — kmod-side ring buffer + sysctl drain for atrium_trace.
 *
 * Defines the storage referenced by atrium_trace.h's ATRIUM_TRACE_KMOD path,
 * plus sysctls:
 *   kern.atrium_trace.enable (rw int) — 0=disabled, 1=enabled
 *   kern.atrium_trace.dump   (ro string) — drains the ring as text:
 *       "<ns_realtime> <cpu> <label>\n"
 *   kern.atrium_trace.reset  (rw int) — write any value to clear the ring
 */
#include <sys/types.h>
#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/sysctl.h>
#include <sys/sbuf.h>
#include <sys/lock.h>
#include <sys/mutex.h>

#define ATRIUM_TRACE_KMOD
#include "atrium_trace.h"

struct atrium_trace_kmod_entry atrium_trace_kmod_ring[ATRIUM_TRACE_KMOD_RING_SIZE];
volatile uint32_t               atrium_trace_kmod_head = 0;
volatile int                    atrium_trace_kmod_enabled = 0;

static SYSCTL_NODE(_kern, OID_AUTO, atrium_trace, CTLFLAG_RW | CTLFLAG_MPSAFE, 0,
    "atrium_trace ring");

SYSCTL_INT(_kern_atrium_trace, OID_AUTO, enable,
    CTLFLAG_RW, __DEVOLATILE(int *, &atrium_trace_kmod_enabled), 0,
    "Enable atrium_trace ring (1=on, 0=off)");

static int
sysctl_atrium_trace_dump(SYSCTL_HANDLER_ARGS)
{
	struct sbuf sb;
	uint32_t head, total, n, start, i;
	int error;

	sbuf_new_for_sysctl(&sb, NULL, 256 * ATRIUM_TRACE_KMOD_RING_SIZE, req);

	head = atrium_trace_kmod_head;
	if (head <= ATRIUM_TRACE_KMOD_RING_SIZE) {
		total = head;
		start = 0;
	} else {
		total = ATRIUM_TRACE_KMOD_RING_SIZE;
		start = head % ATRIUM_TRACE_KMOD_RING_SIZE;
	}

	for (n = 0; n < total; n++) {
		i = (start + n) % ATRIUM_TRACE_KMOD_RING_SIZE;
		struct atrium_trace_kmod_entry *e = &atrium_trace_kmod_ring[i];
		if (e->label[0] == '\0')
			continue;
		sbuf_printf(&sb, "%llu %u %s %llu\n",
		    (unsigned long long)e->ns_realtime,
		    e->cpu, e->label,
		    (unsigned long long)e->id);
	}

	error = sbuf_finish(&sb);
	sbuf_delete(&sb);
	return (error);
}

SYSCTL_PROC(_kern_atrium_trace, OID_AUTO, dump,
    CTLTYPE_STRING | CTLFLAG_RD | CTLFLAG_MPSAFE, NULL, 0,
    sysctl_atrium_trace_dump, "A",
    "Dump atrium_trace ring as text");

static int
sysctl_atrium_trace_reset(SYSCTL_HANDLER_ARGS)
{
	int v = 0;
	int error = sysctl_handle_int(oidp, &v, 0, req);
	if (error || req->newptr == NULL)
		return (error);
	atrium_trace_kmod_head = 0;
	memset(atrium_trace_kmod_ring, 0, sizeof(atrium_trace_kmod_ring));
	return (0);
}

SYSCTL_PROC(_kern_atrium_trace, OID_AUTO, reset,
    CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE, NULL, 0,
    sysctl_atrium_trace_reset, "I",
    "Reset atrium_trace ring (write any value)");
