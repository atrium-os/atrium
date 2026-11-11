// h0 CDM host: drives a Chromium-interface CDM through Initialize and a licence
// request, with an in-memory FileIO, so its imports, syscalls and files can be
// observed. No network: the licence request is printed (size, type), not sent.
// Usage: cdmhost LIB                      (H0: init + licence request with a synthetic PSSH)
//        cdmhost LIB PSSH_B64 DIR         (H0b: licence round trip and real decryption; the
//            driver posts DIR/msg-licreq.bin to a test licence server and writes DIR/resp.bin;
//            DIR/samples.bin from cenc_extract.py is decrypted to DIR/clear.h264)
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
static ContentDecryptionModule_10 *cdmi; static int msgs; static std::string sid, dir = "."; static int keys_usable;
struct H : Host_10 {
  Buffer *Allocate(uint32_t c) override { return new Buf(c); }
  void SetTimer(int64_t ms, void *ctx) override { post(ms / 1000.0, [=]{ cdmi->TimerExpired(ctx); }); }
  Time GetCurrentWallTime() override { timespec t; clock_gettime(CLOCK_REALTIME, &t); return t.tv_sec + t.tv_nsec / 1e9; }
  void OnInitialized(bool ok) override { printf("OnInitialized %d\n", ok); }
  void OnResolveKeyStatusPromise(uint32_t p, KeyStatus s) override { printf("OnResolveKeyStatusPromise %u %u\n", p, s); }
  void OnResolveNewSessionPromise(uint32_t p, const char *id, uint32_t n) override { sid.assign(id, n); printf("OnResolveNewSessionPromise %u %.*s\n", p, n, id); }
  void OnResolvePromise(uint32_t p) override { printf("OnResolvePromise %u\n", p); }
  void OnRejectPromise(uint32_t p, Exception e, uint32_t sc, const char *m, uint32_t n) override { printf("OnRejectPromise %u exc=%u code=%u %.*s\n", p, e, sc, n, m); }
  void OnSessionMessage(const char *id, uint32_t idn, MessageType t, const char *m, uint32_t n) override {
    msgs++; printf("OnSessionMessage type=%u size=%u\n", t, n);
    std::string fn = dir + (t == kIndividualizationRequest ? "/msg-indiv.bin" : "/msg-licreq.bin"); FILE *f = fopen(fn.c_str(), "wb"); if (f) { fwrite(m, 1, n, f); fclose(f); } }
  void OnSessionKeysChange(const char *, uint32_t, bool, const KeyInformation *k, uint32_t n) override { printf("OnSessionKeysChange %u\n", n);
    for (uint32_t i = 0; i < n; i++) { printf("  key "); for (uint32_t j = 0; j < k[i].key_id_size; j++) printf("%02x", k[i].key_id[j]); printf(" status=%u\n", k[i].status); if (k[i].status == kUsable) keys_usable++; } }
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
static void pump(double secs);
static std::vector<uint8_t> slurp(const std::string &p) { std::vector<uint8_t> v; FILE *f = fopen(p.c_str(), "rb"); if (!f) return v;
  fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET); v.resize(n); if (fread(v.data(), 1, n, f) != (size_t)n) v.clear(); fclose(f); return v; }
static int play() {
  std::vector<uint8_t> resp; for (int i = 0; i < 600 && resp.empty(); i++) { pump(0.05); resp = slurp(dir + "/resp.bin"); }
  if (resp.empty()) { printf("no licence response\n"); return 3; }
  printf("UpdateSession with %zu-byte licence\n", resp.size());
  cdmi->UpdateSession(2, sid.data(), sid.size(), resp.data(), resp.size()); pump(2);
  if (!keys_usable) { printf("no usable keys\n"); return 4; }
  std::vector<uint8_t> kid(16); { std::vector<uint8_t> k = slurp(dir + "/kid.hex"); for (int i = 0; i < 16; i++) { char h[3] = {(char)k[2*i], (char)k[2*i+1], 0}; kid[i] = strtol(h, 0, 16); } }
  std::vector<uint8_t> in = slurp(dir + "/samples.bin"), ps = slurp(dir + "/paramsets.h264");
  if (getenv("H0_DECODE")) { // decrypt-and-decode path: the CDM decodes and returns I420 frames
    std::vector<uint8_t> avcc = slurp(dir + "/avcc.bin");
    VideoDecoderConfig_2 vc{}; vc.codec = kCodecH264; vc.profile = kH264ProfileBaseline; vc.format = kI420; vc.coded_size = {192, 144};
    vc.extra_data = avcc.data(); vc.extra_data_size = avcc.size(); vc.encryption_scheme = EncryptionScheme::kCenc;
    printf("InitializeVideoDecoder -> %u\n", cdmi->InitializeVideoDecoder(vc));
    struct VF : VideoFrame { VideoFormat f{}; cdm::Size sz{}; Buffer *b = 0; uint32_t off[3]{}, st[3]{}; int64_t ts = 0;
      void SetFormat(VideoFormat x) override { f = x; } VideoFormat Format() const override { return f; }
      void SetSize(cdm::Size x) override { sz = x; } cdm::Size Size() const override { return sz; }
      void SetFrameBuffer(Buffer *x) override { b = x; } Buffer *FrameBuffer() override { return b; }
      void SetPlaneOffset(VideoPlane p, uint32_t o) override { off[p] = o; } uint32_t PlaneOffset(VideoPlane p) override { return off[p]; }
      void SetStride(VideoPlane p, uint32_t s) override { st[p] = s; } uint32_t Stride(VideoPlane p) override { return st[p]; }
      void SetTimestamp(int64_t t) override { ts = t; } int64_t Timestamp() const override { return ts; } };
    FILE *yuv = fopen((dir + "/cdm-decoded.yuv").c_str(), "wb"); size_t p = 0; int idx = 0; std::map<int, int> stats;
    while (p + 5 <= in.size()) {
      uint32_t sz; memcpy(&sz, &in[p], 4); uint8_t enc = in[p+4]; p += 5; const uint8_t *smp = &in[p]; p += sz;
      uint8_t iv[16]; memcpy(iv, &in[p+1], 16); p += 17; uint32_t ns; memcpy(&ns, &in[p], 4); p += 4;
      std::vector<SubsampleEntry> subs(ns); for (uint32_t i = 0; i < ns; i++) { memcpy(&subs[i].clear_bytes, &in[p], 4); memcpy(&subs[i].cipher_bytes, &in[p+4], 4); p += 8; }
      InputBuffer_2 ib{}; ib.data = smp; ib.data_size = sz; ib.timestamp = idx * 1000;
      if (enc) { ib.encryption_scheme = EncryptionScheme::kCenc; ib.key_id = kid.data(); ib.key_id_size = 16; ib.iv = iv; ib.iv_size = 16; ib.subsamples = ns ? subs.data() : nullptr; ib.num_subsamples = ns; }
      VF vf; Status st = cdmi->DecryptAndDecodeFrame(ib, &vf); stats[st]++;
      if (st == kSuccess && vf.b) { const uint8_t *d = vf.b->Data(); int w = vf.sz.width, h = vf.sz.height;
        for (int y = 0; y < h; y++) fwrite(d + vf.off[0] + y * vf.st[0], 1, w, yuv);
        for (int y = 0; y < h/2; y++) fwrite(d + vf.off[1] + y * vf.st[1], 1, w/2, yuv);
        for (int y = 0; y < h/2; y++) fwrite(d + vf.off[2] + y * vf.st[2], 1, w/2, yuv); vf.b->Destroy(); }
      else if (st != kSuccess && st != kNeedMoreData && stats[st] <= 3) printf("DecryptAndDecodeFrame sample %d -> %u\n", idx, st);
      idx++;
    }
    fclose(yuv); for (auto &kv : stats) printf("status %d: %d\n", kv.first, kv.second);
    cdmi->Destroy(); return stats[kSuccess] ? 0 : 6;
  }
  FILE *out = fopen((dir + "/clear.h264").c_str(), "wb"); fwrite(ps.data(), 1, ps.size(), out);
  size_t p = 0; int ok = 0, fail = 0, clear = 0;
  while (p + 5 <= in.size()) {
    uint32_t sz; memcpy(&sz, &in[p], 4); uint8_t enc = in[p+4]; p += 5; const uint8_t *smp = &in[p]; p += sz;
    uint8_t ivlen = in[p]; uint8_t iv[16]; memcpy(iv, &in[p+1], 16); p += 17; uint32_t ns; memcpy(&ns, &in[p], 4); p += 4;
    std::vector<SubsampleEntry> subs(ns); for (uint32_t i = 0; i < ns; i++) { memcpy(&subs[i].clear_bytes, &in[p], 4); memcpy(&subs[i].cipher_bytes, &in[p+4], 4); p += 8; }
    std::vector<uint8_t> plain;
    if (enc) {
      InputBuffer_2 ib{}; ib.data = smp; ib.data_size = sz; ib.encryption_scheme = EncryptionScheme::kCenc; ib.key_id = kid.data(); ib.key_id_size = 16;
      ib.iv = iv; ib.iv_size = 16; (void)ivlen; ib.subsamples = ns ? subs.data() : nullptr; ib.num_subsamples = ns; ib.timestamp = 0;
      struct DB : DecryptedBlock { Buffer *b = 0; int64_t ts = 0; void SetDecryptedBuffer(Buffer *x) override { b = x; } Buffer *DecryptedBuffer() override { return b; }
        void SetTimestamp(int64_t t) override { ts = t; } int64_t Timestamp() const override { return ts; } } db;
      Status st = cdmi->Decrypt(ib, &db);
      if (st != kSuccess || !db.b) { fail++; static int last = -2; int idx = ok + fail + clear - 1; if (idx != last + 1) printf("Decrypt failed: %u from sample %d (size %u, subsamples %u)\n", st, idx, sz, ns); last = idx; continue; }
      plain.assign(db.b->Data(), db.b->Data() + db.b->Size()); db.b->Destroy(); ok++;
    } else { plain.assign(smp, smp + sz); clear++; }
    for (size_t q = 0; q + 4 <= plain.size();) { uint32_t l = (plain[q] << 24) | (plain[q+1] << 16) | (plain[q+2] << 8) | plain[q+3]; q += 4;
      if (q + l > plain.size()) break; static const uint8_t sc[4] = {0,0,0,1}; fwrite(sc, 1, 4, out); fwrite(&plain[q], 1, l, out); q += l; }
  }
  fclose(out); printf("decrypted=%d failed=%d clear=%d\n", ok, fail, clear);
  cdmi->Destroy(); return fail ? 5 : 0;
}
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
  if (argc > 3) { // H0b: real PSSH from the content
    dir = argv[3]; static const char *b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"; pssh.clear(); int val = 0, bits = -8;
    for (const char *c = argv[2]; *c; c++) { const char *q = strchr(b64, *c); if (!q || *c == '=') continue; val = (val << 6) + (q - b64); bits += 6; if (bits >= 0) { pssh.push_back((val >> bits) & 0xff); bits -= 8; } }
  }
  cdmi->CreateSessionAndGenerateRequest(1, kTemporary, kCenc, pssh.data(), pssh.size()); pump(3);
  if (argc > 3) return play();
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
