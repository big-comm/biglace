# BigLace Mobile — Android project

Kotlin + Jetpack Compose implementation of the plan in the sibling docs
([../ARCHITECTURE.md](../ARCHITECTURE.md), [../ROADMAP.md](../ROADMAP.md),
[../TECH-STACK.md](../TECH-STACK.md), [../UI-DESIGN.md](../UI-DESIGN.md)).

## Status

This is the **M0 + M1-foundation** cut: a real, buildable app that runs the
whole UI shell against a fake backend, with the architecture seams and the
panel contract implemented. Pinned toolchain: AGP 8.12.3 · Kotlin 2.2.20 ·
Gradle 8.14.5 · Compose BOM 2024.12.01 · `compileSdk 36` · `minSdk 26`.

> **The app joins your real Tailscale/Headscale network on-device** via an
> embedded userspace engine (`tsnet`, built from `mobile/tsbridge` with gomobile
> — see below). No separate VPN app, no device-wide VPN permission: only the
> app's own connections (SSH/SFTP) ride the tunnel.

| Area | State |
|---|---|
| App shell: 4-tab nav, Material3 dark-first, dynamic color, real desktop SVG icon | ✅ implemented |
| **Embedded mesh (tsnet)** — join the tailnet in-app, real peer list | ✅ implemented (`tsbridge.aar` + `TsnetMeshBackend`) |
| Peers screen: reactive list of YOUR devices, favorites, online/latency, sort | ✅ implemented |
| **SSH terminal** to a peer, over the tunnel | ✅ implemented (basic — ANSI stripped, password auth; full VT emulator later) |
| **SFTP file browser** of a peer, over the tunnel | ✅ implemented (list + navigate; transfers later) |
| Settings: connection form, save/connect/disconnect, panel sign-in | ✅ implemented |
| `PanelClient`: `preauth-key`, `os-user`, `os-users` — exact desktop contract | ✅ implemented |
| `SettingsStore` / `SecretStore` | ✅ implemented (secrets not encrypted yet — TODO below) |
| Full VT terminal emulator, file transfers, SSH key auth, host-key TOFU UI | ⬜ next — see mobile/ROADMAP.md |

## The embedded engine (mobile/tsbridge)

`mobile/tsbridge/` is a small Go module wrapping Tailscale's `tsnet`. It's built
into `app/libs/tsbridge.aar` with gomobile:

```bash
cd mobile/tsbridge
ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/android-ndk-r27c \
  gomobile bind -target=android/arm64,android/arm -androidapi 26 \
  -javapkg community.biglace -o ../android/app/libs/tsbridge.aar biglace.community/tsbridge
```

The AAR (~30 MB/arch) ships native `libgojni.so` for **arm64-v8a + armeabi-v7a**
only — real phones. x86/x86_64 emulators lack the lib and will fail on Connect.
Kotlin calls `Start / statusJSON / forwardTo / stop`; the status JSON is the same
`ipnstate.Status` shape the desktop parses.

### Deliberate shortcuts in this cut (to keep the first build offline-green)

- **No Hilt / Room / DataStore**: manual `AppContainer` DI + `SharedPreferences`.
  These pull annotation processors (KSP) and weren't in the local cache. Promote
  to Hilt + Room when moving to the multi-module layout (ARCHITECTURE.md §3).
- **`SecretStore` is not encrypted yet.** It uses private-mode
  `SharedPreferences`; migrate to `EncryptedSharedPreferences` + Keystore before
  shipping (ARCHITECTURE.md §8). Marked with a `TODO(security)` in the code.
- **No `sshj` / `Coil` yet** — they belong to M2/M3 and would need network to
  fetch. The SSH/SFTP interfaces are in place so those screens can be built
  against them.
- **State-based navigation** instead of `navigation-compose` (a tab app doesn't
  need it, and it avoided a version-cache mismatch).

## Build

```bash
cd mobile/android
./gradlew assembleDebug          # → app/build/outputs/apk/debug/app-debug.apk
./gradlew installDebug           # onto a connected device/emulator
```

Requires JDK 17 and an Android SDK with platform 36 (`local.properties` →
`sdk.dir`). Open the `mobile/android/` folder in Android Studio for the full
experience (previews, run configs).

## Layout

```
app/src/main/java/org/communitybig/biglace/
├── BigLaceApplication.kt · MainActivity.kt · AppContainer.kt   # entry + DI
├── core/
│   ├── mesh/     MeshBackend, Models (Peer/MeshState), Fake + Companion
│   ├── panel/    PanelClient (preauth-key, os-user, os-users)
│   ├── ssh/      SshManager seam (M2)
│   ├── sftp/     SftpManager seam (M3)
│   └── data/     SettingsStore, SecretStore
├── ui/           BigLaceApp (nav shell), theme/, Common
└── feature/
    ├── peers/    PeersScreen + PeersViewModel
    ├── terminal/ TerminalScreen (scaffold)
    ├── files/    FilesScreen (scaffold)
    └── settings/ SettingsScreen + PanelLoginDialog + SettingsViewModel
```

Packages mirror the intended Gradle modules; split them out (ARCHITECTURE.md §3)
when the terminal/SFTP work makes the build heavy enough to warrant it.

## What you can actually try today

- **Settings**: the connection form and its persistence, and **Sign in with
  panel account** — that dialog makes a *real* HTTP call to a BigScale panel
  (`PanelClient`), so it genuinely fetches a pre-auth key and fills the form.
- **Peers**: renders whatever a backend provides. Since no backend can read the
  live tailnet yet, it shows an honest "not implemented" message instead of the
  old fake sample devices.

The Terminal and Files tabs are scaffolds. Making the app see your real network
is milestone **M4** (embedded Tailscale) — see the note at the top and the
[roadmap](../ROADMAP.md).
