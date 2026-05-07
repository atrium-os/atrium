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

## Architectural decision (2026-05-07, revised)

Initial reading of test 2 was "portcullisd can't be Capsicum'd; lean
on the parent jail." Revised after considering the OpenSSH-style
privsep pattern.

**Final architecture (validated by `jaild-privsep.c`):**

```
host
└── jaild-jail              tiny privileged broker; ONLY caller of
    │                       jail_set(2). ~500 LoC, audited byte-by-byte.
    │                       Validates each request against an allow-list
    │                       of mount sources, devfs rulesets, exec paths.
    │                       Cannot itself be Capsicum'd.
    │
    ├── portcullisd-jail    policy daemon; cap_enter()s after init.
    │                       Opens jaild socket fd at startup, keeps it
    │                       across cap_enter. Sends jail-creation
    │                       requests over the socket; receives jids back.
    │                       Confined by jail + Capsicum.
    │
    ├── frescod             every other Atrium process is a sibling
    ├── atrium-devevents    under jaild-jail, created on portcullisd's
    ├── vestibulum          request via jaild.
    ├── user-N-supervisor
    │   ├── apps
    │   └── ...
    └── ...
```

`jaild-privsep.c` (run on 16.0-CURRENT 2026-05-07) confirms:
- portcullisd in Capsicum CAN read/write a pre-opened socket fd
- portcullisd in Capsicum CANNOT `open()` arbitrary files (`ECAPMODE`)
- portcullisd in Capsicum CANNOT call `jail_set` (`ECAPMODE`)
- jaild-on-the-other-end can call `jail_set` and return the jid
- the request/response round-trip works end-to-end despite confinement
- jaild's allow-list rejects disallowed names with `EPERM`

This is the OpenSSH/qmail privsep pattern. Trade-off accepted: one
extra long-running daemon (jaild) buys us a Capsicum'd portcullisd
plus a tiny audited TCB for the privileged operation. The "only
caller of jail_set" is now ~500 LoC instead of all of portcullisd.

To be folded into `docs/spec/portcullis.md` §4 when D2.5
implementation begins.
