// hospes test CDM: a Clear Key CDM on the real Chromium CDM interface (version 10).
//
// Test code (LANGUAGE-POLICY's pragmatic test clause, docs/spec/hospes.md §11.3). It is
// C++ on purpose: a real C++ compiler decides the vtable layouts that Hospes calls into and
// implements, so a Hospes layout mistake shows up as a wrong answer, not as agreement
// with itself.
//
// What it does:
// - Clear Key sessions: init data `cenc` (ClearKey pssh v1), `keyids` (JSON) and `webm`;
//   the licence request is the W3C JSON form; the response is a JWK set.
// - Persistent-license sessions keep their keys in a FileIO record, so LoadSession and
//   RemoveSession exercise the host's per-origin store.
// - Decrypt: unencrypted, `cenc` (AES-CTR, 8- or 16-byte IV) and `cbcs` (AES-CBC with a
//   pattern, IV reset per subsample), with subsamples.
// - Decrypt-and-decode: only for a made-up "test decoder" (video config extra data
//   "hospes-test-decoder"); the "decoded" I420 frame is a fixed function of the clear
//   bytes. Real codecs are refused, so a player falls back to decrypt-only.
// - Initialize waits for a host timer and asks for output protection status and a
//   storage id first, so those host paths run every time.
//
// No C++ library: no exceptions, no RTTI, no STL. operator new/delete and
// __cxa_pure_virtual are defined here, so the only imports are a handful of libc calls.
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "cdm/content_decryption_module.h"
#include "aes.h"

using namespace cdm;

#define HIDDEN __attribute__((visibility("hidden")))
void *operator new(size_t n) { return malloc(n ? n : 1); }
struct Place {};
inline void *operator new(size_t, Place, void *p) noexcept { return p; }
void operator delete(void *p) noexcept { free(p); }
void operator delete(void *p, size_t) noexcept { free(p); }
extern "C" HIDDEN void __cxa_pure_virtual() { abort(); }

namespace {

const char kKeySystem[] = "org.w3.clearkey";
const uint8_t kClearKeySystemId[16] = {0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02,
                                       0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b};
const char kTestDecoder[] = "hospes-test-decoder";

enum { kMaxKeys = 32, kMaxSessions = 16, kMaxKidsPerRequest = 8 };

// ---- small text helpers -------------------------------------------------------------

struct Str {  // growable byte string on malloc
  char *p = nullptr;
  uint32_t n = 0, cap = 0;
  ~Str() { free(p); }
  void put(const char *s, uint32_t len) {
    if (n + len + 1 > cap) {
      cap = (n + len + 1) * 2;
      p = (char *)realloc(p, cap);
    }
    memcpy(p + n, s, len);
    n += len;
    p[n] = 0;
  }
  void put(const char *s) { put(s, (uint32_t)strlen(s)); }
  void num(uint32_t v) {
    char b[12];
    int i = 11;
    b[i] = 0;
    do { b[--i] = (char)('0' + v % 10); v /= 10; } while (v);
    put(b + i);
  }
};

const char B64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

void b64url(Str &out, const uint8_t *d, uint32_t n) {
  for (uint32_t i = 0; i < n; i += 3) {
    uint32_t v = (uint32_t)d[i] << 16 | (i + 1 < n ? d[i + 1] << 8 : 0) | (i + 2 < n ? d[i + 2] : 0);
    char c[4] = {B64[v >> 18 & 63], B64[v >> 12 & 63], B64[v >> 6 & 63], B64[v & 63]};
    out.put(c, i + 2 < n ? 4 : i + 1 < n ? 3 : 2);
  }
}

// Decode base64url (or plain base64) without padding; returns bytes written, or -1.
int unb64(const char *s, uint32_t n, uint8_t *out, uint32_t cap) {
  uint32_t acc = 0, bits = 0, o = 0;
  for (uint32_t i = 0; i < n; i++) {
    char c = s[i];
    int v = c >= 'A' && c <= 'Z' ? c - 'A' : c >= 'a' && c <= 'z' ? c - 'a' + 26
          : c >= '0' && c <= '9' ? c - '0' + 52 : (c == '-' || c == '+') ? 62
          : (c == '_' || c == '/') ? 63 : c == '=' ? -2 : -1;
    if (v == -2) break;
    if (v < 0) return -1;
    acc = acc << 6 | (uint32_t)v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      if (o == cap) return -1;
      out[o++] = (uint8_t)(acc >> bits);
    }
  }
  return (int)o;
}

// Find the next JSON string value of `"name":"..."` at or after *pos. A deliberately small
// scanner: enough for W3C Clear Key messages, not a JSON parser.
bool json_str(const char *j, uint32_t n, uint32_t *pos, const char *name, const char **v, uint32_t *vn) {
  uint32_t nl = (uint32_t)strlen(name);
  for (uint32_t i = *pos; i + nl + 2 < n; i++) {
    if (j[i] != '"' || memcmp(j + i + 1, name, nl) != 0 || j[i + 1 + nl] != '"') continue;
    uint32_t k = i + nl + 2;
    while (k < n && (j[k] == ' ' || j[k] == ':')) k++;
    if (k >= n || j[k] != '"') continue;
    uint32_t e = k + 1;
    while (e < n && j[e] != '"') e++;
    if (e >= n) return false;
    *v = j + k + 1;
    *vn = e - k - 1;
    *pos = e + 1;
    return true;
  }
  return false;
}

// ---- keys and sessions ----------------------------------------------------------------

struct Key {
  bool used;
  uint8_t kid[16], key[16];
  int session;
  Aes128 aes;
};

struct Session {
  bool used;
  char id[12];
  uint32_t id_len;
  SessionType type;
};

class ClearKeyCdm;

// The FileIO round trips of persistent sessions: one object per operation.
class Record : public FileIOClient {
 public:
  enum Op { kSave, kLoad, kRemove };
  Record(ClearKeyCdm *cdm, Op op, uint32_t promise, int session) : cdm_(cdm), op_(op), promise_(promise), session_(session) {}
  void Start(Host_10 *host, const char *sid, uint32_t sid_len, const uint8_t *data, uint32_t n);
  void OnOpenComplete(Status s) override;
  void OnReadComplete(Status s, const uint8_t *d, uint32_t n) override;
  void OnWriteComplete(Status s) override;

 private:
  void Finish(bool ok, const uint8_t *d, uint32_t n);
  ClearKeyCdm *cdm_;
  Op op_;
  uint32_t promise_;
  int session_;
  FileIO *io_ = nullptr;
  uint8_t *data_ = nullptr;
  uint32_t n_ = 0;
};

class ClearKeyCdm : public ContentDecryptionModule_10 {
 public:
  explicit ClearKeyCdm(Host_10 *host) : host_(host) {}

  void Initialize(bool, bool allow_persistent, bool) override {
    persistent_ok_ = allow_persistent;
    host_->QueryOutputProtectionStatus();
    host_->RequestStorageId(1);
    host_->SetTimer(5, &init_timer_);
  }

  void TimerExpired(void *context) override {
    if (context == &init_timer_) host_->OnInitialized(true);
  }

  void OnQueryOutputProtectionStatus(QueryResult r, uint32_t link_mask, uint32_t prot) override {
    link_mask_ = r == kQuerySucceeded ? link_mask : 0xffffffffu;
    protection_ = prot;
  }

  void OnStorageId(uint32_t, const uint8_t *, uint32_t n) override { storage_id_len_ = n; }

  void OnPlatformChallengeResponse(const PlatformChallengeResponse &) override {}

  void GetStatusForPolicy(uint32_t promise, const Policy &policy) override {
    host_->OnResolveKeyStatusPromise(promise, policy.min_hdcp_version == kHdcpVersionNone ? kUsable : kOutputRestricted);
  }

  void SetServerCertificate(uint32_t promise, const uint8_t *, uint32_t) override {
    Reject(promise, kExceptionNotSupportedError, "server certificates are not used by Clear Key");
  }

  void CreateSessionAndGenerateRequest(uint32_t promise, SessionType type, InitDataType init_type,
                                       const uint8_t *init, uint32_t init_n) override {
    if (type == kPersistentLicense && !persistent_ok_) {
      Reject(promise, kExceptionNotSupportedError, "persistent state not allowed");
      return;
    }
    uint8_t kids[kMaxKidsPerRequest][16];
    int nk = ParseInitData(init_type, init, init_n, kids);
    if (nk <= 0) {
      Reject(promise, kExceptionTypeError, "no Clear Key key ids in the init data");
      return;
    }
    int s = NewSession(type);
    if (s < 0) {
      Reject(promise, kExceptionQuotaExceededError, "too many sessions");
      return;
    }
    Session &ss = sessions_[s];
    host_->OnResolveNewSessionPromise(promise, ss.id, ss.id_len);
    Str msg;
    msg.put("{\"kids\":[");
    for (int i = 0; i < nk; i++) {
      if (i) msg.put(",");
      msg.put("\"");
      b64url(msg, kids[i], 16);
      msg.put("\"");
    }
    msg.put("],\"type\":\"");
    msg.put(type == kPersistentLicense ? "persistent-license" : "temporary");
    msg.put("\",\"hospes\":{\"storage_id_len\":");
    msg.num(storage_id_len_);
    msg.put(",\"link_mask\":");
    msg.num(link_mask_);
    msg.put(",\"protection\":");
    msg.num(protection_);
    msg.put("}}");
    host_->OnSessionMessage(ss.id, ss.id_len, kLicenseRequest, msg.p, msg.n);
  }

  void UpdateSession(uint32_t promise, const char *sid, uint32_t sid_len, const uint8_t *resp, uint32_t n) override {
    int s = FindSession(sid, sid_len);
    if (s < 0) {
      Reject(promise, kExceptionInvalidStateError, "unknown session");
      return;
    }
    // {"keys":[{"kty":"oct","kid":"...","k":"..."}, ...]}: each JWK is a flat object.
    const char *j = (const char *)resp;
    uint32_t added = 0;
    for (uint32_t o = 0; o < n; o++) {
      if (j[o] != '{' || o == 0) continue;  // skip the outer object
      uint32_t e = o + 1;
      while (e < n && j[e] != '}' && j[e] != '{') e++;
      if (e >= n || j[e] != '}') break;
      const char *kid_s, *key_s;
      uint32_t kid_n, key_n, p1 = o, p2 = o;
      bool has_kid = json_str(j, e, &p1, "kid", &kid_s, &kid_n);
      bool has_k = json_str(j, e, &p2, "k", &key_s, &key_n);
      o = e;
      if (!has_kid && !has_k) continue;
      uint8_t kid[16], key[16];
      if (!has_kid || !has_k || unb64(kid_s, kid_n, kid, 16) != 16 || unb64(key_s, key_n, key, 16) != 16) {
        Reject(promise, kExceptionTypeError, "malformed JWK");
        return;
      }
      if (AddKey(s, kid, key) < 0) {
        Reject(promise, kExceptionQuotaExceededError, "too many keys");
        return;
      }
      added++;
    }
    if (!added) {
      Reject(promise, kExceptionTypeError, "no keys in the response");
      return;
    }
    ReportKeys(s, kUsable);
    if (sessions_[s].type == kPersistentLicense) {
      uint8_t buf[kMaxKeys * 32];
      uint32_t bn = SerializeKeys(s, buf);
      (new Record(this, Record::kSave, promise, s))->Start(host_, sid, sid_len, buf, bn);
      return;
    }
    host_->OnResolvePromise(promise);
  }

  void LoadSession(uint32_t promise, SessionType type, const char *sid, uint32_t sid_len) override {
    if (type != kPersistentLicense || !persistent_ok_ || sid_len == 0 || sid_len >= sizeof(Session::id)) {
      Reject(promise, kExceptionTypeError, "only persistent-license sessions load");
      return;
    }
    int s = NewSession(type);
    if (s < 0) {
      Reject(promise, kExceptionQuotaExceededError, "too many sessions");
      return;
    }
    memcpy(sessions_[s].id, sid, sid_len);
    sessions_[s].id[sid_len] = 0;
    sessions_[s].id_len = sid_len;
    (new Record(this, Record::kLoad, promise, s))->Start(host_, sid, sid_len, nullptr, 0);
  }

  void CloseSession(uint32_t promise, const char *sid, uint32_t sid_len) override {
    int s = FindSession(sid, sid_len);
    if (s < 0) {
      Reject(promise, kExceptionInvalidStateError, "unknown session");
      return;
    }
    DropKeys(s);
    sessions_[s].used = false;
    host_->OnSessionClosed(sid, sid_len);
    host_->OnResolvePromise(promise);
  }

  void RemoveSession(uint32_t promise, const char *sid, uint32_t sid_len) override {
    int s = FindSession(sid, sid_len);
    if (s < 0) {
      Reject(promise, kExceptionInvalidStateError, "unknown session");
      return;
    }
    ReportKeys(s, kReleased);
    DropKeys(s);
    if (sessions_[s].type == kPersistentLicense) {
      (new Record(this, Record::kRemove, promise, s))->Start(host_, sid, sid_len, nullptr, 0);
      return;
    }
    host_->OnResolvePromise(promise);
  }

  Status Decrypt(const InputBuffer_2 &in, DecryptedBlock *out) override {
    Buffer *b = host_->Allocate(in.data_size);
    if (!b || b->Capacity() < in.data_size) return kDecryptError;
    Status st = DecryptInto(in, b->Data());
    if (st != kSuccess) {
      b->Destroy();
      return st;
    }
    b->SetSize(in.data_size);
    out->SetDecryptedBuffer(b);
    out->SetTimestamp(in.timestamp);
    return kSuccess;
  }

  Status InitializeAudioDecoder(const AudioDecoderConfig_2 &) override { return kInitializationError; }

  Status InitializeVideoDecoder(const VideoDecoderConfig_2 &c) override {
    if (c.extra_data_size != sizeof(kTestDecoder) - 1 || memcmp(c.extra_data, kTestDecoder, c.extra_data_size) != 0)
      return kInitializationError;
    if (c.coded_size.width <= 0 || c.coded_size.height <= 0 || c.coded_size.width > 4096 ||
        c.coded_size.height > 4096 || (c.coded_size.width | c.coded_size.height) & 1)
      return kInitializationError;
    video_ = c.coded_size;
    video_ready_ = true;
    return kSuccess;
  }

  void DeinitializeDecoder(StreamType t) override {
    if (t == kStreamTypeVideo) video_ready_ = false;
  }

  void ResetDecoder(StreamType) override {}

  Status DecryptAndDecodeFrame(const InputBuffer_2 &in, VideoFrame *frame) override {
    if (!video_ready_) return kDecodeError;
    if (!in.data || !in.data_size) return kNeedMoreData;  // end of stream: nothing buffered
    uint8_t *clear = (uint8_t *)malloc(in.data_size);
    if (!clear) return kDecodeError;
    Status st = DecryptInto(in, clear);
    if (st != kSuccess) {
      free(clear);
      return st;
    }
    uint32_t w = (uint32_t)video_.width, h = (uint32_t)video_.height, ys = w * h, cs = (w / 2) * (h / 2);
    Buffer *b = host_->Allocate(ys + 2 * cs);
    if (!b || b->Capacity() < ys + 2 * cs) {
      free(clear);
      return kDecodeError;
    }
    uint8_t *p = b->Data();
    uint32_t sum = 0;
    for (uint32_t i = 0; i < in.data_size; i++) sum += clear[i];
    for (uint32_t i = 0; i < ys; i++) p[i] = (uint8_t)(clear[i % in.data_size] ^ (i >> 8));
    for (uint32_t i = 0; i < cs; i++) p[ys + i] = (uint8_t)(sum + i);
    for (uint32_t i = 0; i < cs; i++) p[ys + cs + i] = (uint8_t)(in.data_size + i * 7);
    free(clear);
    b->SetSize(ys + 2 * cs);
    frame->SetFormat(kI420);
    frame->SetSize(video_);
    frame->SetFrameBuffer(b);
    frame->SetPlaneOffset(kYPlane, 0);
    frame->SetPlaneOffset(kUPlane, ys);
    frame->SetPlaneOffset(kVPlane, ys + cs);
    frame->SetStride(kYPlane, w);
    frame->SetStride(kUPlane, w / 2);
    frame->SetStride(kVPlane, w / 2);
    frame->SetTimestamp(in.timestamp);
    return kSuccess;
  }

  Status DecryptAndDecodeSamples(const InputBuffer_2 &, AudioFrames *) override { return kDecodeError; }

  void Destroy() override {
    this->~ClearKeyCdm();
    free(this);
  }

  // ---- used by Record ----
  void RecordDone(Record::Op op, uint32_t promise, int s, bool ok, const uint8_t *d, uint32_t n) {
    Session &ss = sessions_[s];
    if (!ok) {
      if (op == Record::kLoad) ss.used = false;
      Reject(promise, kExceptionInvalidStateError, "storage failed");
      return;
    }
    if (op == Record::kSave || op == Record::kRemove) {
      if (op == Record::kRemove) {
        // The session stays (closed by the page later), with no keys.
      }
      host_->OnResolvePromise(promise);
      return;
    }
    // kLoad: an empty or missing record means "no such session" (empty id).
    if (n == 0 || n % 32 != 0) {
      ss.used = false;
      host_->OnResolveNewSessionPromise(promise, "", 0);
      return;
    }
    for (uint32_t i = 0; i + 32 <= n; i += 32) AddKey(s, d + i, d + i + 16);
    host_->OnResolveNewSessionPromise(promise, ss.id, ss.id_len);
    ReportKeys(s, kUsable);
  }

 private:
  void Reject(uint32_t promise, Exception e, const char *m) {
    host_->OnRejectPromise(promise, e, 0, m, (uint32_t)strlen(m));
  }

  int ParseInitData(InitDataType t, const uint8_t *d, uint32_t n, uint8_t kids[][16]) {
    int nk = 0;
    if (t == kWebM) {
      if (n != 16) return 0;
      memcpy(kids[0], d, 16);
      return 1;
    }
    if (t == kKeyIds) {
      uint32_t pos = 0;
      const char *v;
      uint32_t vn;
      // {"kids":["...","..."]}: every quoted string after "kids" is a key id.
      const char *j = (const char *)d;
      uint32_t i = 0;
      for (; i + 6 <= n; i++) if (memcmp(j + i, "\"kids\"", 6) == 0) break;
      if (i + 6 > n) return 0;
      pos = i + 6;
      while (pos < n && nk < kMaxKidsPerRequest) {
        while (pos < n && j[pos] != '"' && j[pos] != ']') pos++;
        if (pos >= n || j[pos] == ']') break;
        v = j + pos + 1;
        uint32_t e = pos + 1;
        while (e < n && j[e] != '"') e++;
        if (e >= n) return 0;
        vn = e - pos - 1;
        if (unb64(v, vn, kids[nk], 16) != 16) return 0;
        nk++;
        pos = e + 1;
      }
      return nk;
    }
    // cenc: a run of pssh boxes; take the Clear Key ones (version 1 lists key ids).
    uint32_t p = 0;
    while (p + 8 <= n) {
      uint32_t size = (uint32_t)d[p] << 24 | d[p + 1] << 16 | d[p + 2] << 8 | d[p + 3];
      if (size < 32 || size > n - p || memcmp(d + p + 4, "pssh", 4) != 0) return 0;
      uint8_t version = d[p + 8];
      if (version == 1 && memcmp(d + p + 12, kClearKeySystemId, 16) == 0) {
        uint32_t q = p + 28;
        if (q + 4 > p + size) return 0;
        uint32_t count = (uint32_t)d[q] << 24 | d[q + 1] << 16 | d[q + 2] << 8 | d[q + 3];
        q += 4;
        if (count > (p + size - q) / 16) return 0;
        for (uint32_t i = 0; i < count && nk < kMaxKidsPerRequest; i++, q += 16) memcpy(kids[nk++], d + q, 16);
      }
      p += size;
    }
    return nk;
  }

  int NewSession(SessionType type) {
    for (int i = 0; i < kMaxSessions; i++) {
      if (sessions_[i].used) continue;
      Session &s = sessions_[i];
      s.used = true;
      s.type = type;
      Str id;
      id.num(++next_session_);
      memcpy(s.id, id.p, id.n + 1);
      s.id_len = id.n;
      return i;
    }
    return -1;
  }

  int FindSession(const char *sid, uint32_t n) {
    for (int i = 0; i < kMaxSessions; i++)
      if (sessions_[i].used && sessions_[i].id_len == n && memcmp(sessions_[i].id, sid, n) == 0) return i;
    return -1;
  }

  int AddKey(int s, const uint8_t kid[16], const uint8_t key[16]) {
    int slot = -1;
    for (int i = 0; i < kMaxKeys; i++) {
      if (keys_[i].used && keys_[i].session == s && memcmp(keys_[i].kid, kid, 16) == 0) { slot = i; break; }
      if (!keys_[i].used && slot < 0) slot = i;
    }
    if (slot < 0) return -1;
    Key &k = keys_[slot];
    k.used = true;
    k.session = s;
    memcpy(k.kid, kid, 16);
    memcpy(k.key, key, 16);
    k.aes.set_key(key);
    return slot;
  }

  void DropKeys(int s) {
    for (int i = 0; i < kMaxKeys; i++) if (keys_[i].used && keys_[i].session == s) keys_[i].used = false;
  }

  uint32_t SerializeKeys(int s, uint8_t *out) {
    uint32_t n = 0;
    for (int i = 0; i < kMaxKeys; i++) {
      if (!keys_[i].used || keys_[i].session != s) continue;
      memcpy(out + n, keys_[i].kid, 16);
      memcpy(out + n + 16, keys_[i].key, 16);
      n += 32;
    }
    return n;
  }

  void ReportKeys(int s, KeyStatus status) {
    KeyInformation info[kMaxKeys];
    uint32_t n = 0;
    for (int i = 0; i < kMaxKeys; i++) {
      if (!keys_[i].used || keys_[i].session != s) continue;
      info[n].key_id = keys_[i].kid;
      info[n].key_id_size = 16;
      info[n].status = status;
      info[n].system_code = 0;
      n++;
    }
    host_->OnSessionKeysChange(sessions_[s].id, sessions_[s].id_len, status == kUsable, info, n);
  }

  const Key *FindKey(const uint8_t *kid, uint32_t n) {
    if (n != 16) return nullptr;
    for (int i = 0; i < kMaxKeys; i++) if (keys_[i].used && memcmp(keys_[i].kid, kid, 16) == 0) return &keys_[i];
    return nullptr;
  }

  // Decrypt `in` into `out` (in.data_size bytes).
  Status DecryptInto(const InputBuffer_2 &in, uint8_t *out) {
    if (in.data_size && !in.data) return kDecryptError;
    if (in.encryption_scheme == EncryptionScheme::kUnencrypted) {
      memcpy(out, in.data, in.data_size);
      return kSuccess;
    }
    const Key *k = FindKey(in.key_id, in.key_id_size);
    if (!k) return kNoKey;
    // The subsample map must cover the sample exactly; no map means all encrypted.
    SubsampleEntry whole = {0, in.data_size};
    const SubsampleEntry *sub = in.num_subsamples ? in.subsamples : &whole;
    uint32_t nsub = in.num_subsamples ? in.num_subsamples : 1;
    uint64_t total = 0;
    for (uint32_t i = 0; i < nsub; i++) total += (uint64_t)sub[i].clear_bytes + sub[i].cipher_bytes;
    if (total != in.data_size) return kDecryptError;

    const uint8_t *src = in.data;
    uint8_t *dst = out;
    if (in.encryption_scheme == EncryptionScheme::kCenc) {
      if (in.iv_size != 8 && in.iv_size != 16) return kDecryptError;
      uint8_t ctr[16] = {0}, ks[16];
      memcpy(ctr, in.iv, in.iv_size);
      uint32_t used = 16;  // keystream bytes consumed from ks
      for (uint32_t i = 0; i < nsub; i++) {
        memcpy(dst, src, sub[i].clear_bytes);
        src += sub[i].clear_bytes;
        dst += sub[i].clear_bytes;
        for (uint32_t b = 0; b < sub[i].cipher_bytes; b++) {
          if (used == 16) {
            k->aes.encrypt(ctr, ks);
            for (int q = 15; q >= 0 && ++ctr[q] == 0; q--) {}
            used = 0;
          }
          *dst++ = *src++ ^ ks[used++];
        }
      }
      return kSuccess;
    }
    if (in.encryption_scheme != EncryptionScheme::kCbcs || in.iv_size != 16) return kDecryptError;
    uint32_t crypt = in.pattern.crypt_byte_block, skip = in.pattern.skip_byte_block;
    if (crypt == 0 && skip == 0) crypt = 1;  // no pattern: every whole block
    if (crypt == 0) return kDecryptError;
    for (uint32_t i = 0; i < nsub; i++) {
      memcpy(dst, src, sub[i].clear_bytes);
      src += sub[i].clear_bytes;
      dst += sub[i].clear_bytes;
      uint8_t chain[16];
      memcpy(chain, in.iv, 16);  // cbcs: the IV restarts at every subsample
      uint32_t left = sub[i].cipher_bytes;
      while (left) {
        for (uint32_t c = 0; c < crypt && left >= 16; c++) {
          uint8_t plain[16], next[16];
          memcpy(next, src, 16);
          k->aes.decrypt(src, plain);
          for (int q = 0; q < 16; q++) dst[q] = plain[q] ^ chain[q];
          memcpy(chain, next, 16);
          src += 16;
          dst += 16;
          left -= 16;
        }
        if (left < 16) {  // a trailing partial block stays clear
          memcpy(dst, src, left);
          src += left;
          dst += left;
          left = 0;
          break;
        }
        uint32_t s = skip * 16 < left ? skip * 16 : left;
        memcpy(dst, src, s);
        src += s;
        dst += s;
        left -= s;
      }
    }
    return kSuccess;
  }

  Host_10 *host_;
  bool persistent_ok_ = false, video_ready_ = false;
  uint32_t storage_id_len_ = 0, link_mask_ = 0, protection_ = 0, next_session_ = 0;
  Size video_ = {0, 0};
  char init_timer_ = 0;
  Key keys_[kMaxKeys] = {};
  Session sessions_[kMaxSessions] = {};
};

void Record::Start(Host_10 *host, const char *sid, uint32_t sid_len, const uint8_t *data, uint32_t n) {
  if (n) {
    data_ = (uint8_t *)malloc(n);
    memcpy(data_, data, n);
    n_ = n;
  }
  io_ = host->CreateFileIO(this);
  if (!io_) {
    Finish(false, nullptr, 0);
    return;
  }
  Str name;
  name.put("ck-");
  name.put(sid, sid_len);
  io_->Open(name.p, name.n);
}

void Record::OnOpenComplete(Status s) {
  if (s != Status::kSuccess) return Finish(false, nullptr, 0);
  if (op_ == kLoad) io_->Read();
  else io_->Write(data_, n_);  // kRemove writes nothing: an empty write deletes the record
}

void Record::OnReadComplete(Status s, const uint8_t *d, uint32_t n) { Finish(s == Status::kSuccess, d, n); }

void Record::OnWriteComplete(Status s) { Finish(s == Status::kSuccess, nullptr, 0); }

void Record::Finish(bool ok, const uint8_t *d, uint32_t n) {
  if (io_) io_->Close();
  io_ = nullptr;
  cdm_->RecordDone(op_, promise_, session_, ok, d, n);
  free(data_);
  delete this;
}

}  // namespace

extern "C" {

CDM_API void INITIALIZE_CDM_MODULE() {}

CDM_API void DeinitializeCdmModule() {}

CDM_API void *CreateCdmInstance(int version, const char *key_system, uint32_t key_system_size,
                                GetCdmHostFunc get_host, void *user_data) {
  if (version != ContentDecryptionModule_10::kVersion) return nullptr;
  if (key_system_size != sizeof(kKeySystem) - 1 || memcmp(key_system, kKeySystem, key_system_size) != 0) return nullptr;
  Host_10 *host = (Host_10 *)get_host(Host_10::kVersion, user_data);
  if (!host) return nullptr;
  void *mem = malloc(sizeof(ClearKeyCdm));
  if (!mem) return nullptr;
  return new (Place{}, mem) ClearKeyCdm(host);
}

CDM_API const char *GetCdmVersion() { return "hospes-test-cdm 1"; }

}  // extern "C"
