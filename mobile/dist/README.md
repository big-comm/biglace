# BigLace Mobile — build artifacts

Versioned APKs of the Android app in [../android/](../android/). Each file name
carries the app version read from the APK itself:

```
biglace-mobile-<versionName>-vc<versionCode>-<buildType>.apk
biglace-mobile-<...>.apk.sha256      # SHA-256 of the APK next to it
```

| File | Version | Build type | Notes |
|---|---|---|---|
| `biglace-mobile-0.9.6-vc33-debug.apk` | 0.9.6 (code 33) | debug | Per-line cached terminal rendering (long Claude Code/TUI sessions stay responsive), VT emulator fixes, cancellable SFTP transfers, host-key change dialog, visible connection errors with plain-language messages. Includes all fixes from 0.9.2. **ARM64 phones only** |

**Debug-signed** — installable for testing (`adb install <file>.apk`) but not for
distribution. A release build needs a signing config (`app/build.gradle.kts`) and
`assembleRelease`.

## Regenerate

```bash
cd ../android
./gradlew assembleDebug
# → app/build/outputs/apk/debug/app-debug.apk  (then copy here, renamed by version)
```

Version comes from `versionName` / `versionCode` in
[`../android/app/build.gradle.kts`](../android/app/build.gradle.kts); bump it
there and rebuild to cut a new artifact.

## Verify

```bash
sha256sum -c biglace-mobile-0.9.6-vc33-debug.apk.sha256
```
