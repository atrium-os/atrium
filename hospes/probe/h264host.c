// h0 OpenH264 host: dlopen Cisco's library, decode an Annex-B file, print an output hash.
// Usage: h264host LIB FILE THREADS
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <stdint.h>
#include "wels/codec_api.h"
typedef long (*create_fn)(ISVCDecoder **);
typedef void (*destroy_fn)(ISVCDecoder *);
static uint64_t h = 1469598103934665603ULL, sum = 0;
static FILE *yuv;
static void fnv(const unsigned char *p, int n) { if (yuv) fwrite(p, 1, n, yuv); for (int i = 0; i < n; i++) { h ^= p[i]; h *= 1099511628211ULL; } }
static void endf(void) { sum += h; h = 1469598103934665603ULL; }
int main(int argc, char **argv) {
  void *lib = dlopen(argv[1], RTLD_NOW); if (!lib) { fprintf(stderr, "%s\n", dlerror()); return 1; }
  create_fn cr = (create_fn)dlsym(lib, "WelsCreateDecoder"); destroy_fn de = (destroy_fn)dlsym(lib, "WelsDestroyDecoder");
  ISVCDecoder *d = 0; if (cr(&d) || !d) return 2;
  if (argc > 4) yuv = fopen(argv[4], "wb");
  int threads = atoi(argv[3]); (*d)->SetOption(d, DECODER_OPTION_NUM_OF_THREADS, &threads);
  SDecodingParam p; memset(&p, 0, sizeof p); p.sVideoProperty.eVideoBsType = VIDEO_BITSTREAM_DEFAULT;
  if ((*d)->Initialize(d, &p)) return 3;
  FILE *f = fopen(argv[2], "rb"); fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
  unsigned char *buf = malloc(n); fread(buf, 1, n, f); fclose(f);
  long i = 0; int frames = 0;
  while (i < n) {
    // gather NALs up to and including one VCL NAL (single-slice pictures): one access unit
    long j = i;
    for (;;) {
      long s = j; while (s + 3 < n && !(buf[s]==0 && buf[s+1]==0 && buf[s+2]==1)) s++;
      int ty = (s + 3 < n) ? (buf[s+3] & 0x1f) : 0;
      long e = s + 3; while (e + 3 < n && !(buf[e]==0 && buf[e+1]==0 && (buf[e+2]==1 || (buf[e+2]==0 && buf[e+3]==1)))) e++;
      if (e + 3 >= n) e = n;
      j = e; if (ty == 1 || ty == 5 || j >= n) break;
    }
    unsigned char *dst[3]; SBufferInfo bi; memset(&bi, 0, sizeof bi);
    if (getenv("H0_DF2")) (*d)->DecodeFrame2(d, buf + i, (int)(j - i), dst, &bi); else (*d)->DecodeFrameNoDelay(d, buf + i, (int)(j - i), dst, &bi);
    if (bi.iBufferStatus == 1) { endf(); frames++; int w = bi.UsrData.sSystemBuffer.iWidth, ht = bi.UsrData.sSystemBuffer.iHeight;
      for (int y = 0; y < ht; y++) fnv(dst[0] + y * bi.UsrData.sSystemBuffer.iStride[0], w);
      for (int y = 0; y < ht/2; y++) fnv(dst[1] + y * bi.UsrData.sSystemBuffer.iStride[1], w/2); for (int y = 0; y < ht/2; y++) fnv(dst[2] + y * bi.UsrData.sSystemBuffer.iStride[1], w/2); }
    i = j;
  }
  { int eos = 1; (*d)->SetOption(d, DECODER_OPTION_END_OF_STREAM, &eos); }
  for (int k = 0; k < 16; k++) { unsigned char *dst[3]; SBufferInfo bi; memset(&bi, 0, sizeof bi);
    (*d)->FlushFrame(d, dst, &bi); if (bi.iBufferStatus != 1) break; endf(); frames++; int w = bi.UsrData.sSystemBuffer.iWidth, ht = bi.UsrData.sSystemBuffer.iHeight;
      for (int y = 0; y < ht; y++) fnv(dst[0] + y * bi.UsrData.sSystemBuffer.iStride[0], w);
      for (int y = 0; y < ht/2; y++) fnv(dst[1] + y * bi.UsrData.sSystemBuffer.iStride[1], w/2); for (int y = 0; y < ht/2; y++) fnv(dst[2] + y * bi.UsrData.sSystemBuffer.iStride[1], w/2); }
  (*d)->Uninitialize(d); de(d);
  endf(); printf("frames=%d framehash-sum=%016llx\n", frames, (unsigned long long)sum);
  return 0;
}
