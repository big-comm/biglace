#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
if [[ -z "$sdk" && -f "$here/../android/local.properties" ]]; then
    sdk="$(sed -n 's/^sdk\.dir=//p' "$here/../android/local.properties" | head -n1)"
fi
[[ -n "$sdk" ]] || { echo "ANDROID_HOME is required" >&2; exit 1; }

ndk="${ANDROID_NDK_HOME:-}"
if [[ -z "$ndk" ]]; then
    ndk="$(find "$sdk/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -n1)"
fi
[[ -d "$ndk" ]] || { echo "Android NDK not found" >&2; exit 1; }

tools="${TMPDIR:-/tmp}/biglace-gomobile-tools"
cache="${TMPDIR:-/tmp}/biglace-gocache"
mkdir -p "$tools" "$cache"
(
    cd "$here"
    GOBIN="$tools" GOCACHE="$cache" go install \
        golang.org/x/mobile/cmd/gomobile \
        golang.org/x/mobile/cmd/gobind
)

output="${1:-$here/../android/app/libs/tsbridge.aar}"
cd "$here"
PATH="$tools:$PATH" \
ANDROID_HOME="$sdk" \
ANDROID_NDK_HOME="$ndk" \
GOCACHE="$cache" \
GOFLAGS="${GOFLAGS:-} -buildvcs=false" \
CGO_LDFLAGS="${CGO_LDFLAGS:-} -Wl,-z,max-page-size=16384" \
gomobile bind -target=android -androidapi=26 -javapkg=community.biglace \
    -ldflags='-s -w' -o "$output" .
