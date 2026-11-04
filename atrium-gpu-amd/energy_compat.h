/*
 * energy_compat.h — the energy-budget federation (P6) lives in the Atrium
 * kernel (<sys/energy_budget.h>). On a stock FreeBSD kernel, such as the
 * amd64 cloud test VM, the modules still build and load: the GPU and display
 * just don't join the federation.
 */
#ifndef ATRIUM_ENERGY_COMPAT_H
#define ATRIUM_ENERGY_COMPAT_H

#if __has_include(<sys/energy_budget.h>)
#include <sys/energy_budget.h>
#else
static inline int
energy_member_register(const char *name,
    uint64_t (*demand)(void *, int),
    void (*actuate)(void *, uint64_t, int), void *arg,
    int flags)
{
	return (-1);
}

static inline void
energy_member_unregister(int id)
{
}
#endif

#endif
