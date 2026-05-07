/*
 * Smoke test for two architectural questions before locking in
 * Atrium's "everything in a jail" plan:
 *
 *   1. Can a jail with children.max>0 create a child jail
 *      via jail_set(2)? (i.e. hierarchical jails work as we hope)
 *
 *   2. Does cap_enter()'s Capsicum capability mode permit
 *      jail_set(2)? If not, portcullisd has to choose between
 *      Capsicum and dynamic jail creation.
 *
 * Run as root in the FreeBSD VM. Each test forks; failure is
 * isolated to its child. Jails are not `persist`-ed, so they
 * vanish when the test child exits — no manual cleanup needed.
 */

#include <sys/param.h>
#include <sys/uio.h>
#include <sys/jail.h>
#include <sys/capsicum.h>
#include <jail.h>
#include <sys/wait.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

static int
add_pair(struct iovec *iov, int *n, const char *key, const void *val, size_t vlen)
{
    iov[*n].iov_base = (void *)key;
    iov[*n].iov_len  = strlen(key) + 1;
    (*n)++;
    iov[*n].iov_base = (void *)val;
    iov[*n].iov_len  = vlen;
    (*n)++;
    return 0;
}

static int
mk_jail(const char *name, const char *path, int children_max, int flags)
{
    struct iovec iov[16];
    char errmsg[256] = {0};
    int n = 0;

    add_pair(iov, &n, "name", name, strlen(name) + 1);
    add_pair(iov, &n, "path", path, strlen(path) + 1);
    add_pair(iov, &n, "children.max", &children_max, sizeof(children_max));
    add_pair(iov, &n, "errmsg", errmsg, sizeof(errmsg));

    int jid = jail_set(iov, n, flags);
    if (jid < 0)
        fprintf(stderr, "  jail_set(name=%s): %s (kernel errmsg='%s')\n",
                name, strerror(errno), errmsg);
    return jid;
}

int
main(void)
{
    int st;
    pid_t pid;

    if (getuid() != 0) {
        fprintf(stderr, "must run as root\n");
        return 2;
    }

    /* ----- Test 1: hierarchical jails ----- */
    printf("=== Test 1: hierarchical jails (parent.children.max=2) ===\n");
    pid = fork();
    if (pid == 0) {
        char pname[64], cname[64];
        snprintf(pname, sizeof(pname), "ptest-%d", getpid());
        snprintf(cname, sizeof(cname), "ctest-%d", getpid());

        if (mk_jail(pname, "/", 2, JAIL_CREATE | JAIL_ATTACH) < 0) {
            fprintf(stderr, "  parent jail creation failed\n");
            _exit(10);
        }
        printf("  [pid %d] now inside parent jail %s\n", getpid(), pname);

        pid_t gc = fork();
        if (gc == 0) {
            if (mk_jail(cname, "/", 0, JAIL_CREATE | JAIL_ATTACH) < 0) {
                fprintf(stderr, "  child jail creation FROM INSIDE parent FAILED\n");
                _exit(11);
            }
            printf("  [pid %d] now inside child jail %s (grandchild of host)\n",
                   getpid(), cname);
            _exit(0);
        }
        waitpid(gc, &st, 0);
        _exit(WEXITSTATUS(st));
    }
    waitpid(pid, &st, 0);
    printf("Test 1: %s\n\n",
           WEXITSTATUS(st) == 0
               ? "PASS — hierarchical jails work; portcullisd-as-jail is viable"
               : "FAIL — see error above");

    /* ----- Test 2: Capsicum + jail_set ----- */
    printf("=== Test 2: cap_enter() then jail_set() ===\n");
    pid = fork();
    if (pid == 0) {
        if (cap_enter() < 0) {
            fprintf(stderr, "  cap_enter: %s\n", strerror(errno));
            _exit(20);
        }
        printf("  entered Capsicum capability mode\n");

        char jname[64];
        snprintf(jname, sizeof(jname), "captest-%d", getpid());
        errno = 0;
        int rc = mk_jail(jname, "/", 0, JAIL_CREATE);
        if (rc < 0) {
            if (errno == ECAPMODE) {
                printf("  jail_set returned -1 errno=ECAPMODE (denied by Capsicum)\n");
                _exit(0);
            }
            printf("  jail_set returned -1 errno=%d (%s) — denied for a different reason\n",
                   errno, strerror(errno));
            _exit(21);
        }
        printf("  jail_set SUCCEEDED in Capsicum mode (jid=%d)\n", rc);
        _exit(22);
    }
    waitpid(pid, &st, 0);
    int rc = WEXITSTATUS(st);
    switch (rc) {
    case 0:
        printf("Test 2: jail_set is BLOCKED in Capsicum mode "
               "→ portcullisd cannot use Capsicum + dynamic jail creation\n");
        break;
    case 22:
        printf("Test 2: jail_set is ALLOWED in Capsicum mode "
               "→ portcullisd CAN combine Capsicum + dynamic jail creation\n");
        break;
    default:
        printf("Test 2: failed, rc=%d (see errors above)\n", rc);
    }

    return 0;
}
