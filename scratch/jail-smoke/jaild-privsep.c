/*
 * Smoke test for the proposed Atrium D2.5 privsep architecture:
 *
 *   jaild        — tiny privileged broker; only thing in the system
 *                  that calls jail_set(2). Cannot itself be Capsicum'd.
 *   portcullisd  — large policy daemon; cap_enter()s after init,
 *                  asks jaild to do jail creation over a pre-opened
 *                  socket fd.
 *
 * Validates:
 *   1. portcullisd CAN read/write the pre-opened socket after
 *      cap_enter() — proves the "open early, cap_enter late" pattern
 *      works for our protocol shape.
 *   2. portcullisd CANNOT open(/etc/passwd) after cap_enter() — proves
 *      Capsicum is actually sealing, not nominal.
 *   3. portcullisd CANNOT call jail_set() after cap_enter() — proves
 *      the privileged operation truly requires the broker.
 *   4. portcullisd's request reaches jaild; jaild creates a jail and
 *      returns a valid jid; portcullisd reads it back — proves the
 *      round-trip works end-to-end despite Capsicum confinement.
 *   5. jaild applies a name allow-list ("smoke-*") and rejects others
 *      — proves the validator-at-broker model works as the policy
 *      enforcement point.
 *
 * Run as root in the FreeBSD VM:
 *   vssh "cc /mnt/host/scratch/jail-smoke/jaild-privsep.c -ljail \
 *          -o /tmp/jaild-privsep && /tmp/jaild-privsep"
 */

#include <sys/param.h>
#include <sys/uio.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <sys/jail.h>
#include <sys/capsicum.h>
#include <jail.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

#define MSG_NAME_MAX 64

struct create_req {
    char name[MSG_NAME_MAX];
};

struct create_resp {
    int jid;
    int err;        /* errno from jail_set, or 0 */
};

static int
mk_jail(const char *name, int persist)
{
    struct iovec iov[16];
    char errmsg[256] = {0};
    int n = 0, cmax = 0;

#define ADD(k, v, vlen) do { \
    iov[n].iov_base = (void *)(k); iov[n].iov_len = strlen(k) + 1; n++; \
    iov[n].iov_base = (void *)(v); iov[n].iov_len = (vlen); n++; \
} while (0)
    ADD("name", name, strlen(name) + 1);
    ADD("path", "/", 2);
    ADD("children.max", &cmax, sizeof(cmax));
    ADD("persist", &persist, sizeof(persist));
    ADD("errmsg", errmsg, sizeof(errmsg));
#undef ADD

    int jid = jail_set(iov, n, JAIL_CREATE);
    if (jid < 0 && errmsg[0])
        fprintf(stderr, "    [kernel errmsg='%s']\n", errmsg);
    return jid;
}

static int
remove_jail(int jid)
{
    return jail_remove(jid);
}

/* jaild: tiny privileged broker. */
static void
jaild_loop(int sock)
{
    fprintf(stderr, "jaild: ready (pid=%d)\n", getpid());
    for (;;) {
        struct create_req req;
        ssize_t n = read(sock, &req, sizeof(req));
        if (n == 0) { fprintf(stderr, "jaild: peer closed\n"); return; }
        if (n != sizeof(req)) {
            fprintf(stderr, "jaild: short read %zd, exit\n", n);
            return;
        }
        req.name[MSG_NAME_MAX - 1] = 0;

        /* Allow-list: only "smoke-*" names are permitted in this test
         * (real jaild's policy is much richer; same shape). */
        struct create_resp resp = { .jid = -1, .err = 0 };
        if (strncmp(req.name, "smoke-", 6) != 0) {
            fprintf(stderr, "jaild: REJECT name='%s' (not smoke-*)\n", req.name);
            resp.err = EPERM;
        } else {
            fprintf(stderr, "jaild: ACCEPT name='%s'; calling jail_set\n",
                    req.name);
            errno = 0;
            int jid = mk_jail(req.name, /*persist=*/1);
            if (jid < 0) {
                resp.err = errno;
                fprintf(stderr, "jaild: jail_set failed: %s\n", strerror(errno));
            } else {
                resp.jid = jid;
                fprintf(stderr, "jaild: jail_set OK, jid=%d (cleanup at "
                        "shutdown)\n", jid);
                /* Real jaild would track jids in a table; for the test we
                 * remove immediately after announcing — proves the round
                 * trip without leaking. */
                if (remove_jail(jid) < 0)
                    fprintf(stderr, "jaild: jail_remove(%d): %s\n",
                            jid, strerror(errno));
            }
        }
        if (write(sock, &resp, sizeof(resp)) != sizeof(resp)) {
            fprintf(stderr, "jaild: write resp: %s\n", strerror(errno));
            return;
        }
    }
}

/* portcullisd-role. */
static int
portcullisd_role(int sock)
{
    /* Step 1: enter Capsicum mode. Socket is already open from
     * fork-inherited socketpair. */
    if (cap_enter() < 0) {
        fprintf(stderr, "portcullisd: cap_enter: %s\n", strerror(errno));
        return 10;
    }
    fprintf(stderr, "portcullisd: in Capsicum mode\n");

    /* Step 2: prove we're really sealed. open() must fail. */
    int fd = open("/etc/passwd", O_RDONLY);
    if (fd >= 0) {
        fprintf(stderr, "portcullisd: open(/etc/passwd) "
                        "UNEXPECTEDLY succeeded\n");
        close(fd);
        return 11;
    }
    if (errno != ECAPMODE) {
        fprintf(stderr, "portcullisd: open() failed with %s, "
                        "expected ECAPMODE\n", strerror(errno));
        return 12;
    }
    fprintf(stderr, "portcullisd: open() correctly denied (ECAPMODE)\n");

    /* Step 3: prove jail_set is also gone for us. */
    if (mk_jail("portcullisd-self", /*persist=*/1) >= 0) {
        fprintf(stderr, "portcullisd: jail_set UNEXPECTEDLY succeeded "
                        "in cap mode\n");
        return 13;
    }
    if (errno != ECAPMODE) {
        fprintf(stderr, "portcullisd: jail_set failed with %s, "
                        "expected ECAPMODE\n", strerror(errno));
        return 14;
    }
    fprintf(stderr, "portcullisd: jail_set correctly denied (ECAPMODE)\n");

    /* Step 4: ask jaild — over the pre-opened socket — to do it. */
    struct create_req req = {0};
    snprintf(req.name, sizeof(req.name), "smoke-%d", getpid());
    if (write(sock, &req, sizeof(req)) != (ssize_t)sizeof(req)) {
        fprintf(stderr, "portcullisd: write req: %s\n", strerror(errno));
        return 15;
    }
    fprintf(stderr, "portcullisd: sent jaild request name='%s'\n", req.name);

    struct create_resp resp;
    if (read(sock, &resp, sizeof(resp)) != (ssize_t)sizeof(resp)) {
        fprintf(stderr, "portcullisd: read resp: %s\n", strerror(errno));
        return 16;
    }
    if (resp.jid < 0) {
        fprintf(stderr, "portcullisd: jaild returned err=%s\n",
                strerror(resp.err));
        return 17;
    }
    fprintf(stderr, "portcullisd: jaild created jail jid=%d on our behalf\n",
            resp.jid);

    /* Step 5: ask jaild to create something OUTSIDE its allow-list,
     * confirm it refuses. */
    struct create_req bad = {0};
    snprintf(bad.name, sizeof(bad.name), "evil-%d", getpid());
    write(sock, &bad, sizeof(bad));
    read(sock, &resp, sizeof(resp));
    if (resp.jid >= 0) {
        fprintf(stderr, "portcullisd: jaild ACCEPTED disallowed name "
                        "(unexpected)\n");
        return 18;
    }
    if (resp.err != EPERM) {
        fprintf(stderr, "portcullisd: jaild rejected with %s, expected EPERM\n",
                strerror(resp.err));
        return 19;
    }
    fprintf(stderr, "portcullisd: jaild correctly REJECTED disallowed name\n");

    fprintf(stderr, "portcullisd: all checks passed\n");
    return 0;
}

int
main(void)
{
    if (getuid() != 0) {
        fprintf(stderr, "must run as root\n");
        return 2;
    }

    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) {
        fprintf(stderr, "socketpair: %s\n", strerror(errno));
        return 1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        fprintf(stderr, "fork: %s\n", strerror(errno));
        return 1;
    }
    if (pid == 0) {
        close(sv[0]);
        jaild_loop(sv[1]);
        _exit(0);
    }
    close(sv[1]);

    int rc = portcullisd_role(sv[0]);
    close(sv[0]);                    /* signals jaild to exit */

    int st;
    waitpid(pid, &st, 0);

    printf("\n=== Result: %s (rc=%d) ===\n",
           rc == 0
               ? "PASS — jaild + Capsicum'd portcullisd works on 16.0-CURRENT"
               : "FAIL — see above",
           rc);
    return rc;
}
