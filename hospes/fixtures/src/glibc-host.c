// Linux reference host for the H4 fixture: runs glibc.so under real glibc (dlopen), as
// `hospes-fixture-runner glibc` runs it under the shim.
#include <dlfcn.h>
#include <stdio.h>
int main(int c, char** v) {
    void* h = dlopen(v[1], RTLD_NOW);
    if (!h) { printf("dlopen: %s\n", dlerror()); return 2; }
    int (*m)(int) = (int (*)(int))dlsym(h, "fixture_glibc_main");
    int r = m(0);
    printf("%s %s (real glibc)\n", r ? "FAIL" : "PASS", v[1]);
    return r;
}
