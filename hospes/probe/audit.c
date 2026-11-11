// h0 audit: counts calls the target library makes to each imported function,
// and logs arguments of dlsym/getenv/syscall/sysconf/getauxval. x86_64 only.
#define _GNU_SOURCE
#include <link.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/mman.h>
#include <stdarg.h>
static uintptr_t target; static unsigned char *pool; static int npool;
static const char *names[1024]; static volatile uint64_t *ctrs[1024];
static FILE *logf_; static void *(*real_dlsym)(void*, const char*); static char *(*real_getenv)(const char*);
static long (*real_syscall)(long, ...); static long (*real_sysconf)(int); static unsigned long (*real_getauxval)(unsigned long);
static FILE *L(void) { if (!logf_) { const char *p = getenv("H0_AUDIT_OUT"); logf_ = fopen(p ? p : "/dev/stderr", "w"); setvbuf(logf_, 0, _IOLBF, 0);} return logf_; }
static void *w_dlsym(void *h, const char *n) { fprintf(L(), "DLSYM handle=%p name=%s\n", h, n); return real_dlsym(h, n); }
static char *w_getenv(const char *n) { fprintf(L(), "GETENV %s\n", n); return real_getenv(n); }
static long w_sysconf(int n) { long r = real_sysconf(n); fprintf(L(), "SYSCONF %d -> %ld\n", n, r); return r; }
static unsigned long w_getauxval(unsigned long t) { unsigned long r = real_getauxval(t); fprintf(L(), "GETAUXVAL %lu -> 0x%lx\n", t, r); return r; }
static long w_syscall(long nr, long a, long b, long c, long d, long e, long f) { fprintf(L(), "SYSCALLFN nr=%ld a0=%ld a1=%ld\n", nr, a, b); return real_syscall(nr, a, b, c, d, e, f); }
unsigned int la_version(unsigned int v) { return LAV_CURRENT; }
unsigned int la_objopen(struct link_map *m, Lmid_t lmid, uintptr_t *cookie) {
  const char *t = getenv("H0_TARGET");
  if (t && m->l_name && strstr(m->l_name, t)) { target = (uintptr_t)cookie; fprintf(L(), "TARGET %s base=%p\n", m->l_name, (void*)m->l_addr); }
  return LA_FLG_BINDTO | LA_FLG_BINDFROM;
}
uintptr_t la_symbind64(Elf64_Sym *sym, unsigned int ndx, uintptr_t *refcook, uintptr_t *defcook, unsigned int *flags, const char *name) {
  if ((uintptr_t)refcook != target || (uintptr_t)defcook == target) return sym->st_value;
  int ty = ELF64_ST_TYPE(sym->st_info);
  fprintf(L(), "BIND %s type=%d\n", name, ty);
  if (ty != STT_FUNC && ty != STT_GNU_IFUNC) return sym->st_value;
  void *real = (void*)sym->st_value;
  if (!strcmp(name, "dlsym")) { real_dlsym = real; real = w_dlsym; }
  else if (!strcmp(name, "getenv")) { real_getenv = real; real = w_getenv; }
  else if (!strcmp(name, "syscall")) { real_syscall = real; real = w_syscall; }
  else if (!strcmp(name, "sysconf")) { real_sysconf = real; real = w_sysconf; }
  else if (!strcmp(name, "getauxval")) { real_getauxval = real; real = w_getauxval; }
  if (!pool) pool = mmap(0, 1024*32, PROT_READ|PROT_WRITE|PROT_EXEC, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
  if (npool >= 1024) return (uintptr_t)real;
  unsigned char *s = pool + npool*32; static const unsigned char code[16] = {0xf0,0x48,0xff,0x05,8,0,0,0, 0xff,0x25,10,0,0,0, 0x90,0x90};
  memcpy(s, code, 16); *(uint64_t*)(s+16) = 0; *(void**)(s+24) = real;
  names[npool] = strdup(name); ctrs[npool] = (uint64_t*)(s+16); npool++;
  return (uintptr_t)s;
}
__attribute__((destructor)) static void dump(void) {
  for (int i = 0; i < npool; i++) fprintf(L(), "CALLS %s %lu\n", names[i], (unsigned long)*ctrs[i]);
}
