# BigLace Mobile — Tech stack

Planning document. Licenses matter here: the desktop app is MIT, and one key
candidate (Termux's terminal widget) is GPLv3 — see § Terminal emulator.

## Core

| Concern | Choice | License | Notes |
|---|---|---|---|
| Language | Kotlin 2.x | Apache-2.0 | Coroutines + Flow everywhere |
| UI | Jetpack Compose + Material 3 | Apache-2.0 | Dynamic color (Monet), predictive back |
| Architecture | MVVM + Hilt | Apache-2.0 | One ViewModel per feature screen |
| Persistence | Room + DataStore | Apache-2.0 | hosts/favorites/known_hosts in Room |
| Secrets | Android Keystore + EncryptedSharedPreferences | Apache-2.0 | see ARCHITECTURE.md § 8 |
| Background work | WorkManager + foreground services | Apache-2.0 | transfer queue, live sessions |
| Min / target SDK | 26 (Android 8.0) / latest | — | 26 keeps notification channels + Keystore sane; raise later if a dependency demands it |

## Mesh layer

| Option | Library | License | Notes |
|---|---|---|---|
| A (v1.0) | `libtailscale` AAR built from [tailscale/tailscale-android](https://github.com/tailscale/tailscale-android) via gomobile | BSD-3-Clause | Pin an upstream tag; prebuild the AAR in CI so app devs don't need Go locally |
| B (MVP) | none — official Tailscale app owns the VPN | — | Peer list via panel API |
| C (alt) | custom gomobile shim over `tsnet` | BSD-3-Clause | Only if VPN-coexistence demand appears |

## SSH + SFTP

**Chosen: [sshj](https://github.com/hierynomus/sshj)** (Apache-2.0).

- One library for shell channels *and* SFTP → shared transport per peer.
- Modern algorithms (ed25519, curve25519-sha256, chacha20-poly1305) via
  Bouncy Castle (MIT-style license).
- Caveats to plan for: register BC as security provider on Android; disable
  the JCE-policy check; keepalive must be configured explicitly.

Alternatives considered:

| Library | License | Why not |
|---|---|---|
| `com.github.mwiede:jsch` (maintained JSch fork) | BSD-3-style | Solid, but separate API styles for exec/sftp; sshj's API is cleaner for channel multiplexing |
| `org.connectbot:sshlib` (trilead fork) | Apache-2.0 | Battle-tested on Android but a dated API; keep as fallback if sshj misbehaves on old devices |
| Apache MINA SSHD | Apache-2.0 | Server-grade, heavy for a mobile client |
| libssh2 via NDK | BSD | JNI maintenance cost not justified |

## Terminal emulator (the license fork in the road)

The emulator = the VT/xterm state machine + the Android view that renders the
grid and handles touch/IME. Writing a good one from scratch is months of work;
the realistic candidates:

| Candidate | License | Quality | Consequence |
|---|---|---|---|
| **Termux `terminal-view` + `terminal-emulator`** | **GPLv3** | Best-in-class on Android: correct xterm handling, fast rendering, battle-tested by Termux | The Android app must be GPLv3-compatible. Doable — ship `mobile/` (or a split `biglace-mobile` repo) under GPLv3 while the desktop stays MIT — but it's a project-level decision |
| ConnectBot emulation stack | Apache-2.0 | Works, but dated rendering and weaker xterm coverage | Keeps everything permissive |
| Custom Compose-canvas emulator | MIT (ours) | Full control, highest effort (escape-sequence coverage is a long tail) | Only worth it if neither option above fits |

**Recommendation**: decide at M2 kickoff. If the team accepts GPLv3 for the
mobile app, take Termux's widget — it is the single biggest shortcut to
"Termius-quality". Verify the exact license text of the `terminal-emulator` /
`terminal-view` modules at adoption time (parts derive from the Apache-2.0
Android Terminal Emulator project; the termux-app repo is GPLv3 overall).

## Terminal look & feel (Termius-style)

- Bundled monospace fonts: JetBrains Mono (OFL-1.1), Fira Code (OFL-1.1),
  Cascadia Code (OFL-1.1) — all redistributable.
- Bundled color schemes: Dracula, Nord, Catppuccin (all MIT), Gruvbox, Solarized
  — stored as JSON assets, user-selectable per host or globally.
- Extra-keys row above the IME: `Esc · Tab · Ctrl · Alt · ← ↓ ↑ → · | · / · ~`,
  long-press for pickers; pinch-to-zoom font size.

## File-manager UI helpers

| Concern | Choice | License |
|---|---|---|
| Image thumbnails | Coil 3 | Apache-2.0 |
| File-type icons | Material Symbols + small custom set | Apache-2.0 |
| Mime detection | Android `MimeTypeMap` + extension table | — |

## Build & CI

- Gradle version catalogs; convention plugins in `build-logic/`.
- CI (GitHub Actions): assemble + unit tests on PR; the `libtailscale` AAR is
  built in a separate pinned workflow and cached as an artifact (mirrors how
  `windows.yml` pins gvsbuild for reproducibility).
- Release: signed AAB; F-Droid-friendly (no proprietary deps in the chosen
  stack — worth preserving when adding libraries).
