#!/bin/sh
# The driver's build identity (gpu/Makefile, gpu-isolation.md): the last commit
# that touched atrium-gpu-amd/ or the shared display code it builds in
# (atrium-display/), plus -dirty for uncommitted changes there.
cd "$(dirname "$0")" || exit 1
h=$(git log -1 --format=%h --abbrev=12 -- . ../atrium-display 2>/dev/null)
if [ -z "$h" ]; then
	echo unknown
elif git diff --quiet HEAD -- . ../atrium-display 2>/dev/null &&
    [ -z "$(git ls-files --others --exclude-standard -- . ../atrium-display 2>/dev/null)" ]; then
	echo "$h"
else
	echo "$h-dirty"
fi
