#!/bin/sh
# Compile section for bench v7: every v7 shader (the SPIR-V the v7 script
# wrote into its work dir) compiled 10 times by RADV (vkCreateComputePipelines,
# no caches) and 10 times by ours (compile-time, atrium main), on this CPU.
# usage: compile-section.sh <v7 work dir> <vk_bench.c> <compile-time binary>
set -e
W=$1; VKC=$2; CT=$3
cc -O2 -I$W/vkdeb/x/usr/include -o "$W/vk_bench_ct" "$VKC" -l:libvulkan.so.1
cd "$W"
echo '=== compile time, ns (RADV: vkCreateComputePipelines, no caches; atrium: SPIR-V to machine code) ==='
grep -m1 'model name' /proc/cpuinfo || true
dpkg-query -W -f='mesa-vulkan-drivers ${Version}\n' mesa-vulkan-drivers 2>/dev/null || true
for n in op_texture_4096 op_rectangle_4096 op_path_4096 op_glyph_run_4096 fresco_bin_1080p fresco_raster_1080p fresco_raster_1080p_v96 fresco_raster_1080p_v64 op_glyph_run_262144_v16 op_path_1048576_v16; do
  case $n in *_v96) v=96;; *_v64) v=64;; *_v16) v=16;; *) v=256;; esac
  echo "--- $n (caps $v/106, spv $(sha256sum $n/shader.spv | cut -c1-12))"
  mkdir -p $n/c; cp $n/shader.spv $n/c/
  sed 's/^repeat .*/repeat 1/; s/^\(buf [0-9]* [0-9]*\) [^ ]* /\1 - /' $n/bench.txt > $n/c/bench.txt
  printf 'compile_repeat 10\ncompile_only 1\n' >> $n/c/bench.txt
  RADV_DEBUG=nocache MESA_SHADER_CACHE_DISABLE=1 ./vk_bench_ct $n/c >/dev/null 2>&1 || true
  echo '[radv]'; grep '^compile_ns_\|^cache_env\|^status\|^error' $n/c/vk_result.txt || true
  echo '[atrium]'; "$CT" $n/shader.spv $v 106 10 | grep -v '^compile_ns=' || true
done
echo '=== end ==='
