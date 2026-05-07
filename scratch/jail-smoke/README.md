# jail-smoke — hierarchical jails + Capsicum

Tiny C smoke test answering two architectural questions for D2.5
Portcullis on FreeBSD 16.0-CURRENT:

1. Can a jail with `children.max>0` create a child jail via
   `jail_set(2)` from inside? (i.e. does the hierarchical-jails
   feature work as documented)
2. Does `cap_enter()`'s Capsicum mode permit `jail_set(2)`?

Run inside the Atrium VM:

```
vssh "cc /mnt/host/scratch/jail-smoke/jail-smoke.c -ljail \
       -o /tmp/jail-smoke && /tmp/jail-smoke"
```

Expected output (recorded 2026-05-07 on `16.0-CURRENT aarch64`):

```
Test 1: PASS — hierarchical jails work; portcullisd-as-jail is viable
Test 2: jail_set is BLOCKED in Capsicum mode
        → portcullisd cannot use Capsicum + dynamic jail creation
```

## Architectural decision (2026-05-07)

`portcullisd` runs inside a parent jail with `children.max>0`,
**not** under Capsicum. The parent jail is the containment boundary;
giving it up would let a portcullisd compromise touch arbitrary
host filesystems, the network stack, kernel modules, etc. Capsicum
would be additional defence-in-depth, but losing dynamic jail
creation is too steep a cost. Pre-allocating a jail pool (so
Capsicum-mode portcullisd could `jail_attach` instead of
`jail_set`) is rejected: caps simultaneous apps, adds bookkeeping,
buys little.

This means: every Atrium process runs jailed. portcullisd's parent
jail must be a superset of all capability mount sources (devices,
ro library trees, aqueduct socket dir, user home tree). Children
inherit subsets. Documented in
`docs/spec/portcullis.md` §4 and in RUNBOOK V7-era architecture
notes.
