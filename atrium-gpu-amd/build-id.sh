#!/bin/sh
# The driver's build identity (gpu/Makefile, gpu-isolation.md): the last commit
# that touched atrium-gpu-amd/, plus -dirty for uncommitted changes there.
cd "$(dirname "$0")" || exit 1
h=$(git log -1 --format=%h --abbrev=12 -- . 2>/dev/null)
if [ -z "$h" ]; then
	echo unknown
elif git diff --quiet HEAD -- . 2>/dev/null &&
    [ -z "$(git ls-files --others --exclude-standard -- . 2>/dev/null)" ]; then
	echo "$h"
else
	echo "$h-dirty"
fi
