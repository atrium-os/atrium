// slangc-lite: forwards slangc arguments (minus -o) to the official Slang
// compiler library and writes the compiled target code to the -o path.
#include <cstdio>
#include <cstring>
#include <vector>
#include "slang.h"
int main(int argc, const char **argv) {
    const char *out = nullptr;
    std::vector<const char *> args;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-o") && i + 1 < argc) { out = argv[++i]; continue; }
        args.push_back(argv[i]);
    }
    slang::IGlobalSession *g = nullptr;
    if (SLANG_FAILED(slang_createGlobalSession(SLANG_API_VERSION, &g))) { fprintf(stderr, "no session\n"); return 5; }
    slang::ICompileRequest *r = nullptr;
    if (SLANG_FAILED(g->createCompileRequest(&r))) { fprintf(stderr, "no request\n"); return 5; }
    if (SLANG_FAILED(r->processCommandLineArguments(args.data(), (int)args.size()))) {
        fprintf(stderr, "%s", r->getDiagnosticOutput()); return 2;
    }
    SlangResult res = r->compile();
    const char *d = r->getDiagnosticOutput();
    if (d && *d) fprintf(stderr, "%s", d);
    if (SLANG_FAILED(res)) return 1;
    ISlangBlob *blob = nullptr;
    if (SLANG_FAILED(r->getTargetCodeBlob(0, &blob)) || !blob) {
        if (SLANG_FAILED(r->getEntryPointCodeBlob(0, 0, &blob)) || !blob) { fprintf(stderr, "no code\n"); return 3; }
    }
    FILE *f = out ? fopen(out, "wb") : stdout;
    if (!f) { perror(out); return 4; }
    fwrite(blob->getBufferPointer(), 1, blob->getBufferSize(), f);
    if (out) fclose(f);
    return 0;
}
