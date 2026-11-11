// h0 tracer: counts syscalls by (calling module, syscall number), logs opened paths,
// exec-mprotects and signal handlers. x86_64 Linux only. Usage: tracer OUT prog args...
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <errno.h>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <sys/user.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#define MAXT 4096
static int insys[1<<16];
struct ent { char mod[128]; long nr; long n; } ents[8192]; int nents;
static FILE *out;
static void modof(pid_t tid, unsigned long a, char *mod) {
  char p[64], line[512]; snprintf(p, sizeof p, "/proc/%d/maps", tid);
  FILE *f = fopen(p, "r"); strcpy(mod, "?"); if (!f) return;
  while (fgets(line, sizeof line, f)) { unsigned long lo, hi; char perm[8]; int off=0;
    if (sscanf(line, "%lx-%lx %7s %*s %*s %*s %n", &lo, &hi, perm, &off) >= 3 && a >= lo && a < hi) {
      char *n = line + off; n[strcspn(n, "\n")] = 0; char *b = strrchr(n, '/');
      snprintf(mod, 128, "%s%s", b ? b+1 : (*n ? n : "[anon]"), strchr(perm,'x') ? "" : "(nx)"); break; } }
  fclose(f);
}
static void count(const char *mod, long nr) {
  for (int i = 0; i < nents; i++) if (ents[i].nr == nr && !strcmp(ents[i].mod, mod)) { ents[i].n++; return; }
  if (nents < 8192) { strcpy(ents[nents].mod, mod); ents[nents].nr = nr; ents[nents].n = 1; nents++; }
}
static void rdstr(pid_t t, unsigned long a, char *buf, int n) {
  int i = 0; while (i < n-8) { errno = 0; long w = ptrace(PTRACE_PEEKDATA, t, a+i, 0); if (errno) break;
    memcpy(buf+i, &w, 8); if (memchr(&w, 0, 8)) break; i += 8; } buf[n-1] = 0;
}
int main(int argc, char **argv) {
  out = fopen(argv[1], "w");
  pid_t c = fork();
  if (!c) { ptrace(PTRACE_TRACEME, 0, 0, 0); raise(SIGSTOP); execvp(argv[2], argv+2); _exit(127); }
  int st; waitpid(c, &st, 0);
  ptrace(PTRACE_SETOPTIONS, c, 0, PTRACE_O_TRACESYSGOOD|PTRACE_O_TRACECLONE|PTRACE_O_TRACEFORK|PTRACE_O_TRACEVFORK|PTRACE_O_TRACEEXEC|PTRACE_O_EXITKILL);
  ptrace(PTRACE_SYSCALL, c, 0, 0);
  int live = 1, rc = 0;
  while (live > 0) {
    pid_t t = waitpid(-1, &st, __WALL); if (t < 0) break;
    if (WIFEXITED(st) || WIFSIGNALED(st)) { live--; if (t == c) rc = WIFEXITED(st) ? WEXITSTATUS(st) : 128+WTERMSIG(st); continue; }
    int sig = 0;
    if (WIFSTOPPED(st)) {
      int s = WSTOPSIG(st);
      if (s == (SIGTRAP|0x80)) {
        int k = t & 0xffff; insys[k] ^= 1;
        if (insys[k]) {
          struct user_regs_struct r; ptrace(PTRACE_GETREGS, t, 0, &r);
          char mod[128]; modof(t, r.rip - 2, mod); long nr = r.orig_rax; count(mod, nr);
          if (nr == SYS_openat || nr == SYS_open || nr == SYS_creat || nr == SYS_newfstatat || nr == SYS_stat || nr == SYS_readlink || nr == SYS_access || nr == SYS_statx) {
            char p[512]; unsigned long a = (nr == SYS_openat || nr == SYS_newfstatat || nr == SYS_statx) ? r.rsi : r.rdi;
            rdstr(t, a, p, sizeof p); fprintf(out, "PATH %s nr=%ld flags=0x%llx from=%s\n", p, nr, nr==SYS_openat? r.rdx : r.rsi, mod); }
          if ((nr == SYS_mprotect || nr == SYS_mmap) && (r.rdx & PROT_EXEC)) fprintf(out, "EXEC nr=%ld len=%llu prot=%llu from=%s\n", nr, r.rsi, r.rdx, mod);
          if (nr == SYS_rt_sigaction) fprintf(out, "SIGACTION sig=%llu set=%d from=%s\n", r.rdi, r.rsi != 0, mod);
          if (nr == SYS_prctl) fprintf(out, "PRCTL op=%llu from=%s\n", r.rdi, mod);
        }
      } else if (s == SIGTRAP && (st >> 16)) { if ((st>>16) == PTRACE_EVENT_CLONE || (st>>16) == PTRACE_EVENT_FORK || (st>>16) == PTRACE_EVENT_VFORK) live++; }
      else if (s == SIGSTOP) { }
      else sig = s;
    }
    ptrace(PTRACE_SYSCALL, t, 0, sig);
  }
  for (int i = 0; i < nents; i++) fprintf(out, "SYS %s %ld %ld\n", ents[i].mod, ents[i].nr, ents[i].n);
  fclose(out); return rc;
}
