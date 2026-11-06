#!/bin/sh
# Make atrium-player's test clips and ffmpeg's reference frame hashes.
# ffmpeg is a dev tool here, never shipped (LICENSING-POLICY.md: tools).
# Usage: sh scripts/media-test-clips.sh [outdir]   (default test-assets/media)
set -eu
out=${1:-$(dirname "$0")/../test-assets/media}
mkdir -p "$out"
cd "$out"
ff="ffmpeg -hide_banner -loglevel error -y"
src="-f lavfi -i testsrc2=size=160x90:rate=24:duration=2 -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2"
# WebM: the codecs YouTube serves.
$ff $src -c:v libvpx-vp9 -b:v 200k -g 24 -c:a libopus -b:a 48k vp9-opus.webm
$ff $src -c:v libaom-av1 -cpu-used 8 -b:v 200k -g 24 -c:a libopus -b:a 48k av1-opus.webm
$ff $src -c:v libvpx -b:v 200k -g 24 -c:a libvorbis -q:a 2 vp8-vorbis.webm
# 10-bit (P010 path).
$ff $src -pix_fmt yuv420p10le -c:v libvpx-vp9 -profile:v 2 -b:v 200k -g 24 -an vp9-10bit.webm
# MP4: plain (moov sample tables) and fragmented (what MSE feeds use).
$ff $src -c:v libaom-av1 -cpu-used 8 -b:v 200k -g 24 -c:a libopus -b:a 48k -strict -2 av1-opus.mp4
$ff $src -c:v libvpx-vp9 -b:v 200k -g 24 -c:a libopus -b:a 48k -strict -2 \
    -movflags frag_keyframe+empty_moov+default_base_moof vp9-opus-frag.mp4
# Colour fields for the on-screen check (atrium-player M4): 1 s red, then 1 s blue.
$ff -f lavfi -i color=c=red:s=160x96:r=24:d=1 -f lavfi -i color=c=blue:s=160x96:r=24:d=1 \
    -filter_complex '[0][1]concat=n=2:v=1:a=0' -c:v libvpx-vp9 -b:v 100k -g 24 red-blue.webm
# Colour descriptions (atrium-player M7): HDR10 tags in VP9/WebM, full-range BT.709 in
# AV1/MP4 (colr box and the sequence header).
c="-f lavfi -i testsrc2=size=64x36:rate=24:duration=0.5"
$ff $c -pix_fmt yuv420p10le -c:v libvpx-vp9 -profile:v 2 -b:v 50k -color_primaries bt2020 \
    -color_trc smpte2084 -colorspace bt2020nc -color_range tv -an color-hdr10.webm
$ff $c -c:v libaom-av1 -cpu-used 8 -b:v 50k -color_primaries bt709 -color_trc bt709 \
    -colorspace bt709 -color_range pc -an color-full.mp4
# Reference hashes of every decoded frame (planar, as the decoder outputs).
for f in *.webm *.mp4; do
    $ff -i "$f" -map 0:v -f framemd5 "$f.video.framemd5"
    # What atrium-player puts in a video layer: NV12, or P010 for 10-bit.
    case $f in *10bit*) pf=p010le ;; *) pf=nv12 ;; esac
    $ff -i "$f" -map 0:v -pix_fmt $pf -f framemd5 "$f.layer.framemd5"
    # The video packets as the container states them: pts (s), key flag.
    ffprobe -v error -select_streams v -show_entries packet=pts_time,flags -of csv=p=0 "$f" \
        > "$f.video.packets"
done
ls -l
