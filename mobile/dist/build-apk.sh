#!/usr/bin/env bash
# Build the release APKs (one per ABI) and stage them here, named by version:
#   biglace-mobile-<versionName>-vc<versionCode>-<abi>.apk (+ .sha256)
# Only the newest build is kept. CI (.github/workflows/android.yml) runs this
# too and attaches the result to the release; the in-app updater looks for
# exactly these file names. Extra Gradle arguments come from $GRADLE_ARGS.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
android="$here/../android"

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
if [[ -z "$sdk" && -f "$android/local.properties" ]]; then
    sdk="$(sed -n 's/^sdk\.dir=//p' "$android/local.properties" | head -n1)"
fi
aapt2="$(find "$sdk/build-tools" -name aapt2 -type f 2>/dev/null | sort -V | tail -n1)"
[[ -x "$aapt2" ]] || { echo "aapt2 not found (set ANDROID_HOME)" >&2; exit 1; }

# shellcheck disable=SC2086 # GRADLE_ARGS is a list of -P options
(cd "$android" && ./gradlew --quiet ${GRADLE_ARGS:-} :app:assembleRelease)

out="$android/app/build/outputs/apk/release"
shopt -s nullglob
apks=("$out"/app-*-release.apk)
[[ ${#apks[@]} -gt 0 ]] || { echo "no signed release APK in $out" >&2; exit 1; }

rm -f "$here"/biglace-mobile-*.apk "$here"/biglace-mobile-*.apk.sha256
for apk in "${apks[@]}"; do
    abi="$(basename "$apk" | sed -E 's/^app-(.+)-release\.apk$/\1/')"
    # Read everything, then keep line 1: `| head` would SIGPIPE aapt2 under pipefail.
    badging="$("$aapt2" dump badging "$apk")"
    badging="${badging%%$'\n'*}"
    name="$(sed -E "s/.*versionName='([^']+)'.*/\1/" <<<"$badging")"
    code="$(sed -E "s/.*versionCode='([0-9]+)'.*/\1/" <<<"$badging")"
    dest="biglace-mobile-$name-vc$code-$abi.apk"
    cp "$apk" "$here/$dest"
    (cd "$here" && sha256sum "$dest" > "$dest.sha256")
    echo "$here/$dest"
done
