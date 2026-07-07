# BigLace Mobile — build artifacts

Versioned APKs of the Android app in [../android/](../android/). Each file name
carries the app version read from the APK itself:

```
biglace-mobile-<versionName>-vc<versionCode>-<buildType>.apk
biglace-mobile-<...>.apk.sha256      # SHA-256 of the APK next to it
```

| File | Version | Build type | Notes |
|---|---|---|---|
| `biglace-mobile-0.7.5-vc23-debug.apk` | 0.7.5 (code 23) | debug | Auth form now **follows the selected mode**: **Key** hides the password (username only); **Password**/**Auto** show it. The working **username is remembered per host** and prefilled next time (the guessed default — the peer hostname — is often not the OS login, e.g. `ihxfrank` not `francois-asus`). Plus 0.7.4: CLR runs `clear`, no input/keyboard gap; and the rest of 0.7.x. **ARM phones only** |
| `biglace-mobile-0.5.2-vc14-debug.apk` | 0.5.2 (code 14) | debug | Fixes the Android netlink block (feeds interfaces from Java) so the tsnet connection actually comes up; connect from the Peers tab with clear feedback; tap a peer → terminal/SFTP direct; polished UI. **ARM phones only** |

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
sha256sum -c biglace-mobile-0.7.5-vc23-debug.apk.sha256
```
