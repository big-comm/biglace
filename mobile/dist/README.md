# BigLace Mobile — build artifacts

Release APKs of the Android app in [../android/](../android/), one per CPU
architecture. Only the newest build is kept here:

```
biglace-mobile-<versionName>-vc<versionCode>-<abi>.apk
biglace-mobile-<...>.apk.sha256      # SHA-256 of the APK next to it
```

| Files | Version | Build type | Notes |
|---|---|---|---|
| `biglace-mobile-0.9.7-vc34-<abi>.apk` | 0.9.7 (code 34) | release | In-app updates from GitHub Releases (Settings → Updates, plus a quiet check at launch). PgUp/PgDn keys, plain-language connection errors, per-line terminal rendering so long Claude Code/TUI sessions stay responsive, cancellable SFTP transfers, host-key change dialog. |

Most phones need `arm64-v8a`; `armeabi-v7a` is for old 32-bit phones and the
`x86*` builds are for emulators.

## Build

```bash
./build-apk.sh
```

Bump `versionName` / `versionCode` in
[`../android/app/build.gradle.kts`](../android/app/build.gradle.kts) first.
The script runs `assembleRelease`, copies the APKs here renamed by version,
writes the checksums and removes the previous build.

**Signing.** In-app updates only install over the current app when every APK is
signed with the same key. Builds use the key in `../android/keystore.properties`
(gitignored: `storeFile`, `storePassword`, `keyAlias`, `keyPassword`) when that
file exists, and this machine's debug key otherwise — which is what the 0.9.x
builds so far carry. Switching keys means users uninstall once; losing the key
means no more in-app updates. Back it up.

## Releases (CI)

A gitrepo build publishes the release through BigCommunity's build-package,
which fires the `windows-msi-build` dispatch. Both
[`windows.yml`](../../.github/workflows/windows.yml) and
[`android.yml`](../../.github/workflows/android.yml) answer it, so the Linux
package, the Windows MSI and the APKs end up in the same release. The Android
job runs `build-apk.sh` with the version taken from the tag (`YY.MM.DD-HHMM` →
versionName as-is, versionCode = minutes since 2020-01-01) and needs the
signing key as repository secrets: `ANDROID_KEYSTORE_BASE64`,
`ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`.
Without them the job fails rather than ship APKs nobody could update to.

The app looks at the repository's releases and picks the APK for the device's
architecture by its file name, so keep the naming intact.

## Verify

```bash
sha256sum -c biglace-mobile-0.9.7-vc34-*.apk.sha256
```
