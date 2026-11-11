// h0 CDM host: drives a Chromium-interface CDM through Initialize and a licence
// request, with an in-memory FileIO, so its imports, syscalls and files can be
// observed. No network: the licence request is printed (size, type), not sent.
// Usage: cdmhost LIB
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <time.h>
#include <mutex>
#include <deque>
#include <functional>
#include <map>
#include <string>
#include <vector>
#include "cdmapi/content_decryption_module.h"
using namespace cdm;
static std::mutex mu; static std::deque<std::pair<double, std::function<void()>>> q;
static double now() { timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static void post(double delay, std::function<void()> f) { std::lock_guard<std::mutex> g(mu); q.push_back({now() + delay, f}); }
static std::map<std::string, std::vector<uint8_t>> store;
struct Buf : Buffer { std::vector<uint8_t> v; uint32_t sz = 0;
  explicit Buf(uint32_t c) : v(c) {} void Destroy() override { delete this; }
  uint32_t Capacity() const override { return v.size(); } uint8_t* Data() override { return v.data(); }
  void SetSize(uint32_t s) override { sz = s; } uint32_t Size() const override { return sz; } };
struct FIO : FileIO { FileIOClient *c; std::string name;
  explicit FIO(FileIOClient *c) : c(c) {}
  void Open(const char *n, uint32_t len) override { name.assign(n, len); printf("  FileIO.Open %s\n", name.c_str()); post(0, [=]{ c->OnOpenComplete(FileIOClient::Status::kSuccess); }); }
  void Read() override { auto d = store[name]; printf("  FileIO.Read %s -> %zu\n", name.c_str(), d.size()); post(0, [=]{ c->OnReadComplete(FileIOClient::Status::kSuccess, d.data(), d.size()); }); }
  void Write(const uint8_t *d, uint32_t n) override { store[name].assign(d, d + n); printf("  FileIO.Write %s %u\n", name.c_str(), n); post(0, [=]{ c->OnWriteComplete(FileIOClient::Status::kSuccess); }); }
  void Close() override { delete this; } };
static ContentDecryptionModule_10 *cdmi; static int msgs;
struct H : Host_10 {
  Buffer *Allocate(uint32_t c) override { return new Buf(c); }
  void SetTimer(int64_t ms, void *ctx) override { post(ms / 1000.0, [=]{ cdmi->TimerExpired(ctx); }); }
  Time GetCurrentWallTime() override { timespec t; clock_gettime(CLOCK_REALTIME, &t); return t.tv_sec + t.tv_nsec / 1e9; }
  void OnInitialized(bool ok) override { printf("OnInitialized %d\n", ok); }
  void OnResolveKeyStatusPromise(uint32_t p, KeyStatus s) override { printf("OnResolveKeyStatusPromise %u %u\n", p, s); }
  void OnResolveNewSessionPromise(uint32_t p, const char *id, uint32_t n) override { printf("OnResolveNewSessionPromise %u %.*s\n", p, n, id); }
  void OnResolvePromise(uint32_t p) override { printf("OnResolvePromise %u\n", p); }
  void OnRejectPromise(uint32_t p, Exception e, uint32_t sc, const char *m, uint32_t n) override { printf("OnRejectPromise %u exc=%u code=%u %.*s\n", p, e, sc, n, m); }
  void OnSessionMessage(const char *id, uint32_t idn, MessageType t, const char *m, uint32_t n) override {
    msgs++; printf("OnSessionMessage type=%u size=%u\n", t, n);
    FILE *f = fopen(t == kIndividualizationRequest ? "msg-indiv.bin" : "msg-licreq.bin", "wb"); if (f) { fwrite(m, 1, n, f); fclose(f); } }
  void OnSessionKeysChange(const char *, uint32_t, bool, const KeyInformation *, uint32_t n) override { printf("OnSessionKeysChange %u\n", n); }
  void OnExpirationChange(const char *, uint32_t, Time t) override { printf("OnExpirationChange %f\n", t); }
  void OnSessionClosed(const char *, uint32_t) override { printf("OnSessionClosed\n"); }
  void SendPlatformChallenge(const char *, uint32_t, const char *, uint32_t) override { printf("SendPlatformChallenge\n");
    post(0, [=]{ PlatformChallengeResponse r{}; cdmi->OnPlatformChallengeResponse(r); }); }
  void EnableOutputProtection(uint32_t m) override { printf("EnableOutputProtection %u\n", m); }
  void QueryOutputProtectionStatus() override { printf("QueryOutputProtectionStatus\n"); post(0, [=]{ cdmi->OnQueryOutputProtectionStatus(kQuerySucceeded, 0, 0); }); }
  void OnDeferredInitializationDone(StreamType, Status) override {}
  FileIO *CreateFileIO(FileIOClient *c) override { printf("CreateFileIO\n"); return new FIO(c); }
  void RequestStorageId(uint32_t v) override { printf("RequestStorageId %u\n", v); post(0, [=]{ static uint8_t id[32] = {1,2,3}; cdmi->OnStorageId(1, id, 32); }); }
} host;
static void *gethost(int v, void *) { printf("GetCdmHost version %d\n", v); return v == 10 ? (void*)&host : nullptr; }
static void pump(double secs) { double end = now() + secs;
  while (now() < end) { std::function<void()> f; { std::lock_guard<std::mutex> g(mu);
      for (auto it = q.begin(); it != q.end(); ++it) if (it->first <= now()) { f = it->second; q.erase(it); break; } }
    if (f) f(); else { timespec ts{0, 5000000}; nanosleep(&ts, 0); } } }
int main(int argc, char **argv) {
  void *lib = dlopen(argv[1], RTLD_NOW); if (!lib) { printf("dlopen: %s\n", dlerror()); return 1; }
  auto init = (void(*)())dlsym(lib, "InitializeCdmModule_4"); auto ver = (const char*(*)())dlsym(lib, "GetCdmVersion");
  auto create = (void*(*)(int, const char*, uint32_t, GetCdmHostFunc, void*))dlsym(lib, "CreateCdmInstance");
  init(); printf("GetCdmVersion %s\n", ver());
  const char *ks = "com.widevine.alpha";
  cdmi = (ContentDecryptionModule_10*)create(10, ks, strlen(ks), gethost, nullptr); printf("CreateCdmInstance(10) -> %p\n", (void*)cdmi);
  if (!cdmi) return 2;
  cdmi->Initialize(false, false, false); pump(1);
  // A minimal Widevine PSSH: pssh v0, Widevine system id, WidevinePsshData { algorithm: AESCTR, key_id: 16 bytes }
  uint8_t data[] = {0x08,0x01,0x12,0x10, 0x10,0x11,0x12,0x13,0x14,0x15,0x16,0x17,0x18,0x19,0x1a,0x1b,0x1c,0x1d,0x1e,0x1f};
  uint8_t sys[] = {0xed,0xef,0x8b,0xa9,0x79,0xd6,0x4a,0xce,0xa3,0xc8,0x27,0xdc,0xd5,0x1d,0x21,0xed};
  std::vector<uint8_t> pssh; uint32_t total = 32 + sizeof data;
  auto be32 = [&](uint32_t x){ for (int i = 3; i >= 0; i--) pssh.push_back(x >> (i*8)); };
  be32(total); pssh.insert(pssh.end(), {'p','s','s','h'}); be32(0); pssh.insert(pssh.end(), sys, sys + 16); be32(sizeof data); pssh.insert(pssh.end(), data, data + sizeof data);
  cdmi->CreateSessionAndGenerateRequest(1, kTemporary, kCenc, pssh.data(), pssh.size()); pump(3);
  // Decrypt with no key: exercises the decrypt path to its kNoKey answer.
  uint8_t enc[64] = {0}, kid[16] = {0x10,0x11,0x12,0x13,0x14,0x15,0x16,0x17,0x18,0x19,0x1a,0x1b,0x1c,0x1d,0x1e,0x1f}, iv[16] = {0};
  SubsampleEntry ss{16, 48}; InputBuffer_2 in{}; in.data = enc; in.data_size = 64; in.encryption_scheme = EncryptionScheme::kCenc;
  in.key_id = kid; in.key_id_size = 16; in.iv = iv; in.iv_size = 16; in.subsamples = &ss; in.num_subsamples = 1; in.timestamp = 0;
  struct DB : DecryptedBlock { Buffer *b = 0; int64_t ts = 0; void SetDecryptedBuffer(Buffer *x) override { b = x; } Buffer *DecryptedBuffer() override { return b; }
    void SetTimestamp(int64_t t) override { ts = t; } int64_t Timestamp() const override { return ts; } } out;
  printf("Decrypt -> %u\n", cdmi->Decrypt(in, &out)); pump(1);
  cdmi->Destroy(); auto deinit = (void(*)())dlsym(lib, "DeinitializeCdmModule"); deinit();
  printf("messages=%d\n", msgs); return 0;
}
