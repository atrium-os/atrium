#!/bin/sh
# Clear Key test content for Hospes H3 (docs/spec/hospes.md §11.3), made from the clear
# clips in test-assets/media. Needs GPAC's MP4Box (Ubuntu package `gpac`), ffmpeg and
# python3-cryptography. The decoded output of each file equals its clear source's, so the
# source's .framemd5 files are the reference.
#
#   vp9-opus-cenc.mp4  fragmented, cenc (AES-CTR, VP9 subsamples), ClearKey pssh
#   av1-opus-cbcs.mp4  progressive, cbcs (pattern 1:9 video, constant IVs), ClearKey pssh
#   vp9-opus-enc.webm  WebM Encryption: partitioned, whole-frame and clear frames
#
# Key id 00112233445566778899aabbccddeeff, key 3a2b1c0d4e5f60718293a4b5c6d7e8f9 (test only).
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SRC="$ROOT/test-assets/media"
OUT="$SRC/clearkey"
KID=00112233445566778899aabbccddeeff
KEY=3a2b1c0d4e5f60718293a4b5c6d7e8f9
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$OUT"

drm() { # scheme-type extra-attributes-for-track
    cat <<X
<?xml version="1.0" encoding="UTF-8"?>
<GPACDRM type="$1">
<DRMInfo type="pssh" version="1">
<BS ID128="1077efecc0b24d02ace33c1e52e2fb4b"/>
<BS bits="32" value="1"/>
<BS ID128="$KID"/>
</DRMInfo>
<CrypTrack trackID="1" IsEncrypted="1" first_IV="0x0a610676cb88f302" saiSavedBox="senc" $2>
<key KID="0x$KID" value="0x$KEY"/>
</CrypTrack>
<CrypTrack trackID="2" IsEncrypted="1" first_IV="0x1b720787dc99f413" saiSavedBox="senc" $3>
<key KID="0x$KID" value="0x$KEY"/>
</CrypTrack>
</GPACDRM>
X
}

drm "CENC AES-CTR" 'IV_size="8"' 'IV_size="8"' > "$T/cenc.xml"
drm "CENC AES-CBC Pattern" \
    'constant_IV_size="16" constant_IV="0x9fb1a3d4c1e2f30415263748596a7b8c" crypt_byte_block="1" skip_byte_block="9"' \
    'constant_IV_size="16" constant_IV="0x8ea092c3b0d1e2f3041526374859a6b7" crypt_byte_block="1" skip_byte_block="9"' > "$T/cbcs.xml"

# cenc, fragmented: encrypt, then fragment through GPAC's DASH muxer (-crypt with -frag in
# one step writes clear output, and ffmpeg cannot read GPAC's -frag senc).
MP4Box -quiet -crypt "$T/cenc.xml" "$SRC/vp9-opus-frag.mp4" -out "$T/vflat.mp4"
(cd "$T" && MP4Box -quiet -dash 1000 -frag 1000 -profile onDemand -out v.mpd vflat.mp4)
cp "$T/vflat_dashinit.mp4" "$OUT/vp9-opus-cenc.mp4"

MP4Box -quiet -crypt "$T/cbcs.xml" "$SRC/av1-opus.mp4" -out "$OUT/av1-opus-cbcs.mp4"

python3 "$ROOT/scripts/webm-encrypt.py" "$SRC/vp9-opus.webm" "$OUT/vp9-opus-enc.webm" "$KID" "$KEY"

# Check the MP4s against their sources with ffmpeg's own CENC decryption.
for p in vp9-opus-cenc:vp9-opus-frag av1-opus-cbcs:av1-opus; do
    e=${p%%:*}; c=${p#*:}
    a=$(ffmpeg -v error -decryption_key $KEY -i "$OUT/$e.mp4" -f framemd5 - | grep -v '^#' | awk -F', *' '{print $NF}')
    b=$(ffmpeg -v error -i "$SRC/$c.mp4" -f framemd5 - | grep -v '^#' | awk -F', *' '{print $NF}')
    [ "$a" = "$b" ] || { echo "$e: decrypts differently from $c" >&2; exit 1; }
    echo "$e.mp4: decrypts to $c.mp4" >&2
done
