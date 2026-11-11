/* Loader fixtures for Hospes H1 (docs/spec/hospes.md §11.1). Built -nostdlib: every import
 * comes from the fixture runner's host table, so the loader is tested on its own, before
 * the glibc shim exists. fixture_main returns 0, or the number of the first failed check. */
#ifndef HOSPES_FIXTURE_H
#define HOSPES_FIXTURE_H
typedef unsigned long uptr;
extern void host_log(const char *msg);
extern int host_add(int a, int b);
extern int host_data[8];
extern uptr host_thread_create(uptr (*fn)(uptr), uptr arg);
extern uptr host_thread_join(uptr handle);
extern uptr host_canary(void);
extern int host_os(void); /* 1 FreeBSD, 0 Linux */
#define CHECK(n, cond) do { if (!(cond)) return (n); } while (0)
#define EXPORT __attribute__((visibility("default")))
#endif
