// What refhost and hospes-cdm-check share: the vectors file and the fixed inputs of the
// test scenario (docs/spec/hospes.md §11.3). Test code.
#pragma once
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <string>
#include <vector>

namespace scen {

const char kKeySystem[] = "org.w3.clearkey";
const char kTestDecoder[] = "hospes-test-decoder";
const uint8_t kKid1[16] = {0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff};
const uint8_t kKid2[16] = {0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf};

struct Vec {
  uint8_t op, scheme, expect;
  uint32_t crypt, skip;
  int64_t ts;
  uint8_t kid[16];
  std::vector<uint8_t> iv;
  std::vector<uint32_t> subs;  // clear, cipher pairs
  std::vector<uint8_t> data, clear;
};

struct Data {
  std::vector<Vec> vecs;
  std::string licence, licence1;
};

inline std::vector<uint8_t> slurp(const std::string &p) {
  std::vector<uint8_t> v;
  FILE *f = fopen(p.c_str(), "rb");
  if (!f) return v;
  uint8_t b[65536];
  size_t n;
  while ((n = fread(b, 1, sizeof b, f)) > 0) v.insert(v.end(), b, b + n);
  fclose(f);
  return v;
}

inline bool load(const std::string &dir, Data &d) {
  std::vector<uint8_t> v = slurp(dir + "/vectors.bin");
  std::vector<uint8_t> l = slurp(dir + "/licence.json"), l1 = slurp(dir + "/licence-kid1.json");
  if (v.empty() || l.empty() || l1.empty()) return false;
  d.licence.assign(l.begin(), l.end());
  d.licence1.assign(l1.begin(), l1.end());
  size_t p = 0;
  auto need = [&](size_t n) { return p + n <= v.size(); };
  auto u32 = [&]() { uint32_t x; memcpy(&x, &v[p], 4); p += 4; return x; };
  while (p < v.size()) {
    Vec r;
    if (!need(24)) return false;
    r.op = v[p]; r.scheme = v[p + 1];
    uint8_t ivn = v[p + 2];
    r.expect = v[p + 3];
    p += 4;
    r.crypt = u32(); r.skip = u32();
    memcpy(&r.ts, &v[p], 8); p += 8;
    if (!need(16 + ivn + 4)) return false;
    memcpy(r.kid, &v[p], 16); p += 16;
    r.iv.assign(v.begin() + p, v.begin() + p + ivn); p += ivn;
    uint32_t ns = u32();
    if (!need(8ull * ns + 4)) return false;
    for (uint32_t i = 0; i < 2 * ns; i++) r.subs.push_back(u32());
    uint32_t n = u32();
    if (!need(n + 4ull)) return false;
    r.data.assign(v.begin() + p, v.begin() + p + n); p += n;
    n = u32();
    if (!need(n)) return false;
    r.clear.assign(v.begin() + p, v.begin() + p + n); p += n;
    d.vecs.push_back(r);
  }
  return true;
}

// One ClearKey pssh (version 1) naming both test keys.
inline std::vector<uint8_t> clearkey_pssh() {
  static const uint8_t sys[16] = {0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02, 0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b};
  std::vector<uint8_t> b = {0, 0, 0, 0, 'p', 's', 's', 'h', 1, 0, 0, 0};
  b.insert(b.end(), sys, sys + 16);
  b.insert(b.end(), {0, 0, 0, 2});
  b.insert(b.end(), kKid1, kKid1 + 16);
  b.insert(b.end(), kKid2, kKid2 + 16);
  b.insert(b.end(), {0, 0, 0, 0});  // no data
  b[3] = (uint8_t)b.size();
  return b;
}

inline std::string keyids_json() { return "{\"kids\":[\"ABEiM0RVZneImaq7zN3u_w\"]}"; }

inline uint64_t fnv(const uint8_t *d, size_t n) {
  uint64_t h = 0xcbf29ce484222325ull;
  for (size_t i = 0; i < n; i++) { h ^= d[i]; h *= 0x100000001b3ull; }
  return h;
}

// The record names hospes-cdm accepts (spec §8.4).
inline bool record_name_ok(const char *n, uint32_t len) {
  if (len == 0 || len > 64) return false;
  for (uint32_t i = 0; i < len; i++) {
    char c = n[i];
    if (!((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-')) return false;
  }
  return true;
}

}  // namespace scen
