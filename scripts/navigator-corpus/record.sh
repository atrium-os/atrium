#!/bin/sh
# Record a batch of pages for the Navigator corpus.
#
#   scripts/navigator-corpus/record.sh <urls.tsv> <out-dir>
#
# <urls.tsv> holds "<stem><TAB><url>" lines (# comments allowed). Each page is
# fetched once with curl, then converted by navigator-prerender with the
# network on and exploration enabled, exactly as the 2026-09-24 corpus was:
#
#   <out-dir>/html/        the fetched pages and manifest.tsv (file -> URL)
#   <out-dir>/recordings/  one recording per page that the origin served
#   <out-dir>/blobs/       image bytes by BLAKE3 address
#   <out-dir>/fetch.txt    HTTP status per URL
#   <out-dir>/report.txt   the converter's batch report
#
# Each page gets PRERENDER_DEADLINE seconds (default here 180): fetching a
# news front page's scripts one by one easily outruns the converter's 30.
#
# ★ The output is OTHER SITES' CONTENT. Point <out-dir> outside the repo
# (~/src/navigator-corpus/...) and never commit it.
set -eu
[ $# -eq 2 ] || { echo "usage: $0 <urls.tsv> <out-dir>" >&2; exit 2; }
list=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
out=$2
root=$(cd "$(dirname "$0")/../.." && pwd)
case "$(cd "$(dirname "$out")" 2>/dev/null && pwd)/" in
    "$root"/*) echo "refusing: $out is inside the repository" >&2; exit 2 ;;
esac
mkdir -p "$out/html" "$out/recordings" "$out/blobs"
out=$(cd "$out" && pwd)
ua="atrium-navigator-corpus/0.1 (measurement)"
: > "$out/html/manifest.tsv"
: > "$out/fetch.txt"
tab=$(printf '\t')
while IFS="$tab" read -r stem url; do
    case "$stem" in ''|\#*) continue ;; esac
    code=$(curl -sSL --compressed --max-time 40 -A "$ua" -o "$out/html/$stem.html" -w '%{http_code}' "$url" 2>/dev/null) || code=000
    printf '%s\t%s\t%s\n' "$code" "$stem" "$url" >> "$out/fetch.txt"
    if [ "$code" = 200 ]; then
        printf '%s.html\t%s\n' "$stem" "$url" >> "$out/html/manifest.tsv"
    else
        echo "fetch $code: $url" >&2
        rm -f "$out/html/$stem.html"
    fi
done < "$list"
(cd "$root/navigator-prerender" && cargo build --release --quiet)
PRERENDER_DEADLINE="${PRERENDER_DEADLINE:-180}" PRERENDER_NET=1 PRERENDER_EXPLORE=1 PRERENDER_EMIT_DIR="$out/recordings" PRERENDER_BLOB_DIR="$out/blobs" \
    "$root/navigator-prerender/target/release/prerender" "$out/html" > "$out/report.txt" 2>&1 || true
echo "fetched $(grep -c '^200' "$out/fetch.txt") of $(wc -l < "$out/fetch.txt" | tr -d ' '); recorded $(ls "$out/recordings" | wc -l | tr -d ' ')"
