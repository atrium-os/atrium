// The Linux reference host for the test CDM (docs/spec/hospes.md §11.3). Test code.
//
//   refhost LIB VECTORS_DIR
//
// Loads a Chromium-interface CDM with glibc's dlopen, runs the fixed scenario below and
// prints a transcript. `hospes-cdm-check` runs the same scenario through hospes-cdm and
// must print the same transcript, line for line.
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <deque>
#include <functional>
#include <map>
#include <string>
#include <vector>

#include "cdm/content_decryption_module.h"
#include "scenario.h"

using namespace cdm;

static std::deque<std::function<void()>> queue;
struct Timer { double at; void *ctx; };
static std::vector<Timer> timers;
static ContentDecryptionModule_10 *cdmi;
static std::map<std::string, std::vector<uint8_t>> store;

static double mono() { timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

static std::string hex(const uint8_t *d, uint32_t n) {
  std::string s; char b[3];
  for (uint32_t i = 0; i < n; i++) { snprintf(b, 3, "%02x", d[i]); s += b; }
  return s;
}

struct Buf : Buffer {
  std::vector<uint8_t> v; uint32_t sz = 0;
  explicit Buf(uint32_t c) : v(c) {}
  void Destroy() override { delete this; }
  uint32_t Capacity() const override { return v.size(); }
  uint8_t *Data() override { return v.data(); }
  void SetSize(uint32_t s) override { sz = s; }
  uint32_t Size() const override { return sz; }
};

struct Block : DecryptedBlock {
  Buffer *b = nullptr; int64_t ts = 0;
  void SetDecryptedBuffer(Buffer *x) override { b = x; }
  Buffer *DecryptedBuffer() override { return b; }
  void SetTimestamp(int64_t t) override { ts = t; }
  int64_t Timestamp() const override { return ts; }
};

struct Frame : VideoFrame {
  VideoFormat fmt = kUnknownVideoFormat; cdm::Size size = {0, 0}; Buffer *b = nullptr;
  uint32_t off[3] = {0}, stride[3] = {0}; int64_t ts = 0;
  void SetFormat(VideoFormat f) override { fmt = f; }
  VideoFormat Format() const override { return fmt; }
  void SetSize(cdm::Size s) override { size = s; }
  cdm::Size Size() const override { return size; }
  void SetFrameBuffer(Buffer *x) override { b = x; }
  Buffer *FrameBuffer() override { return b; }
  void SetPlaneOffset(VideoPlane p, uint32_t o) override { if (p < 3) off[p] = o; }
  uint32_t PlaneOffset(VideoPlane p) override { return p < 3 ? off[p] : 0; }
  void SetStride(VideoPlane p, uint32_t s) override { if (p < 3) stride[p] = s; }
  uint32_t Stride(VideoPlane p) override { return p < 3 ? stride[p] : 0; }
  void SetTimestamp(int64_t t) override { ts = t; }
  int64_t Timestamp() const override { return ts; }
};

struct FIO : FileIO {
  FileIOClient *c; std::string name;
  explicit FIO(FileIOClient *c) : c(c) {}
  void Open(const char *n, uint32_t len) override {
    name.assign(n, len);
    bool ok = scen::record_name_ok(n, len);
    queue.push_back([=] { c->OnOpenComplete(ok ? FileIOClient::Status::kSuccess : FileIOClient::Status::kError); });
  }
  void Read() override {
    auto d = store.count(name) ? store[name] : std::vector<uint8_t>();
    queue.push_back([=] { c->OnReadComplete(FileIOClient::Status::kSuccess, d.data(), d.size()); });
  }
  void Write(const uint8_t *d, uint32_t n) override {
    if (n) store[name].assign(d, d + n); else store.erase(name);
    queue.push_back([=] { c->OnWriteComplete(FileIOClient::Status::kSuccess); });
  }
  void Close() override { delete this; }
};

struct H : Host_10 {
  Buffer *Allocate(uint32_t c) override { return new Buf(c); }
  void SetTimer(int64_t ms, void *ctx) override { timers.push_back({mono() + ms / 1000.0, ctx}); }
  Time GetCurrentWallTime() override { timespec t; clock_gettime(CLOCK_REALTIME, &t); return t.tv_sec + t.tv_nsec / 1e9; }
  void OnInitialized(bool ok) override { printf("initialized %d\n", ok); }
  void OnResolveKeyStatusPromise(uint32_t p, KeyStatus s) override { printf("key-status p=%u s=%u\n", p, s); }
  void OnResolveNewSessionPromise(uint32_t p, const char *id, uint32_t n) override { printf("new-session p=%u id=%.*s\n", p, n, id); }
  void OnResolvePromise(uint32_t p) override { printf("resolve p=%u\n", p); }
  void OnRejectPromise(uint32_t p, Exception e, uint32_t sc, const char *m, uint32_t n) override {
    printf("reject p=%u e=%u code=%u msg=%.*s\n", p, e, sc, n, m);
  }
  void OnSessionMessage(const char *id, uint32_t idn, MessageType t, const char *m, uint32_t n) override {
    printf("message id=%.*s type=%u %.*s\n", idn, id, t, n, m);
  }
  void OnSessionKeysChange(const char *id, uint32_t idn, bool add, const KeyInformation *k, uint32_t n) override {
    printf("keys id=%.*s new=%d", idn, id, add);
    for (uint32_t i = 0; i < n; i++) printf(" %s=%u/%u", hex(k[i].key_id, k[i].key_id_size).c_str(), k[i].status, k[i].system_code);
    printf("\n");
  }
  void OnExpirationChange(const char *id, uint32_t n, Time) override { printf("expiration id=%.*s\n", n, id); }
  void OnSessionClosed(const char *id, uint32_t n) override { printf("closed id=%.*s\n", n, id); }
  void SendPlatformChallenge(const char *, uint32_t, const char *, uint32_t) override {
    queue.push_back([] { PlatformChallengeResponse r{}; cdmi->OnPlatformChallengeResponse(r); });
  }
  void EnableOutputProtection(uint32_t) override {}
  void QueryOutputProtectionStatus() override { queue.push_back([] { cdmi->OnQueryOutputProtectionStatus(kQuerySucceeded, 0, 0); }); }
  void OnDeferredInitializationDone(StreamType, Status) override {}
  FileIO *CreateFileIO(FileIOClient *c) override { return new FIO(c); }
  void RequestStorageId(uint32_t v) override {
    queue.push_back([v] { static uint8_t id[32]; for (int i = 0; i < 32; i++) id[i] = (uint8_t)(i * 37 + 11); cdmi->OnStorageId(v, id, 32); });
  }
} host;

static void *get_host(int v, void *) { return v == Host_10::kVersion ? (void *)&host : nullptr; }

// Run queued callbacks and due timers until nothing is left (or 2 s pass).
static void settle() {
  double end = mono() + 2;
  while (mono() < end) {
    if (!queue.empty()) { auto f = queue.front(); queue.pop_front(); f(); continue; }
    if (timers.empty()) return;
    size_t k = 0;
    for (size_t i = 1; i < timers.size(); i++) if (timers[i].at < timers[k].at) k = i;
    double wait = timers[k].at - mono();
    if (wait > 0) { timespec t = {0, (long)(wait * 1e9)}; nanosleep(&t, nullptr); }
    void *ctx = timers[k].ctx; timers.erase(timers.begin() + k);
    cdmi->TimerExpired(ctx);
  }
}

static InputBuffer_2 input(const scen::Vec &v) {
  InputBuffer_2 in{};
  in.data = v.data.data(); in.data_size = v.data.size();
  in.encryption_scheme = (EncryptionScheme)v.scheme;
  in.key_id = v.kid; in.key_id_size = 16;
  in.iv = v.iv.data(); in.iv_size = v.iv.size();
  in.subsamples = (const SubsampleEntry *)v.subs.data(); in.num_subsamples = v.subs.size() / 2;
  in.pattern = {v.crypt, v.skip};
  in.timestamp = v.ts;
  return in;
}

static void decrypt(size_t i, const scen::Vec &v) {
  InputBuffer_2 in = input(v);
  Block b;
  Status s = cdmi->Decrypt(in, &b);
  uint32_t n = b.b ? b.b->Size() : 0;
  // ok: the status is the expected one, and on success the bytes are the clear ones.
  bool ok = s == (Status)v.expect && (s != kSuccess || (n == v.clear.size() && memcmp(b.b->Data(), v.clear.data(), n) == 0));
  printf("decrypt i=%zu status=%u ts=%lld len=%u ok=%d\n", i, s, (long long)(s == kSuccess ? b.ts : 0), n, ok);
  if (b.b) b.b->Destroy();
}

static void decode(size_t i, const scen::Vec *v) {
  InputBuffer_2 in{};
  if (v) in = input(*v);
  Frame f;
  Status s = cdmi->DecryptAndDecodeFrame(in, &f);
  if (s != kSuccess || !f.b) {
    printf("frame i=%zu status=%u\n", i, s);
  } else {
    printf("frame i=%zu status=0 fmt=%u size=%dx%d off=%u,%u,%u stride=%u,%u,%u ts=%lld fnv=%016llx\n", i, f.fmt, f.size.width,
           f.size.height, f.off[0], f.off[1], f.off[2], f.stride[0], f.stride[1], f.stride[2], (long long)f.ts,
           (unsigned long long)scen::fnv(f.b->Data(), f.b->Size()));
  }
  if (f.b) f.b->Destroy();
}

int main(int argc, char **argv) {
  if (argc != 3) { fprintf(stderr, "usage: refhost LIB VECTORS_DIR\n"); return 2; }
  setvbuf(stdout, nullptr, _IOLBF, 0);
  scen::Data d;
  if (!scen::load(argv[2], d)) { fprintf(stderr, "refhost: cannot read vectors in %s\n", argv[2]); return 2; }
  void *lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
  if (!lib) { fprintf(stderr, "refhost: %s\n", dlerror()); return 2; }
  auto init = (void (*)())dlsym(lib, "InitializeCdmModule_4");
  auto create = (void *(*)(int, const char *, uint32_t, GetCdmHostFunc, void *))dlsym(lib, "CreateCdmInstance");
  auto version = (const char *(*)())dlsym(lib, "GetCdmVersion");
  auto deinit = (void (*)())dlsym(lib, "DeinitializeCdmModule");
  if (!init || !create || !version || !deinit) { fprintf(stderr, "refhost: missing entry points\n"); return 2; }
  init();
  printf("version %s\n", version());
  cdmi = (ContentDecryptionModule_10 *)create(10, scen::kKeySystem, strlen(scen::kKeySystem), get_host, nullptr);
  if (!cdmi) { printf("create failed\n"); return 1; }

  // The scenario; hospes-cdm-check (Rust) follows these steps in the same order.
  cdmi->Initialize(false, true, false); settle();
  cdmi->GetStatusForPolicy(1, Policy{kHdcpVersionNone}); settle();
  cdmi->GetStatusForPolicy(2, Policy{kHdcpVersion2_2}); settle();
  cdmi->SetServerCertificate(3, (const uint8_t *)"x", 1); settle();
  std::vector<uint8_t> pssh = scen::clearkey_pssh();
  cdmi->CreateSessionAndGenerateRequest(4, kTemporary, kCenc, pssh.data(), pssh.size()); settle();
  cdmi->UpdateSession(5, "1", 1, (const uint8_t *)d.licence.data(), d.licence.size()); settle();
  for (size_t i = 0; i < d.vecs.size(); i++) if (d.vecs[i].op == 1) decrypt(i, d.vecs[i]);
  AudioDecoderConfig_2 ac{kCodecVorbis, 2, 16, 48000, nullptr, 0, EncryptionScheme::kCenc};
  printf("audio-init status=%u\n", cdmi->InitializeAudioDecoder(ac));
  VideoDecoderConfig_2 vc{};
  vc.codec = kCodecVp9; vc.profile = kVP9Profile0; vc.format = kI420; vc.coded_size = {64, 48};
  vc.encryption_scheme = EncryptionScheme::kCenc;
  printf("video-init status=%u\n", cdmi->InitializeVideoDecoder(vc));
  vc.extra_data = (uint8_t *)scen::kTestDecoder; vc.extra_data_size = strlen(scen::kTestDecoder);
  printf("video-init status=%u\n", cdmi->InitializeVideoDecoder(vc));
  for (size_t i = 0; i < d.vecs.size(); i++) if (d.vecs[i].op == 2) decode(i, &d.vecs[i]);
  decode(d.vecs.size(), nullptr);
  cdmi->ResetDecoder(kStreamTypeVideo);
  cdmi->DeinitializeDecoder(kStreamTypeVideo);
  std::string kids = scen::keyids_json();
  cdmi->CreateSessionAndGenerateRequest(6, kPersistentLicense, kKeyIds, (const uint8_t *)kids.data(), kids.size()); settle();
  cdmi->UpdateSession(7, "2", 1, (const uint8_t *)d.licence1.data(), d.licence1.size()); settle();
  cdmi->CloseSession(8, "2", 1); settle();
  cdmi->CloseSession(9, "1", 1); settle();
  decrypt(1, d.vecs[1]);
  cdmi->LoadSession(10, kPersistentLicense, "2", 1); settle();
  decrypt(1, d.vecs[1]);
  cdmi->LoadSession(11, kPersistentLicense, "99", 2); settle();
  cdmi->RemoveSession(12, "2", 1); settle();
  cdmi->CloseSession(13, "2", 1); settle();
  cdmi->LoadSession(14, kPersistentLicense, "2", 1); settle();
  cdmi->UpdateSession(15, "7", 1, (const uint8_t *)"{}", 2); settle();
  cdmi->Destroy();
  deinit();
  printf("end\n");
  return 0;
}
