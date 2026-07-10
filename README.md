<div align="center">

<img src="usr/share/icons/hicolor/scalable/apps/org.communitybig.biglace.svg" width="128" alt="BigLace">

# BigLace

**Mesh VPN client with a graphical interface for Headscale and BigScale.**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20Android-blue)](#platforms)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-orange?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![GTK4 · Libadwaita](https://img.shields.io/badge/GTK4-Libadwaita-blueviolet?logo=gnome&logoColor=white)](https://gtk.org)
[![Latest release](https://img.shields.io/github/v/release/big-comm/biglace?label=release&color=brightgreen)](https://github.com/big-comm/biglace/releases)

</div>

---

BigLace provides native desktop and Android clients for joining a **BigScale**
mesh (or any plain Headscale server). The GTK4 desktop app drives the local
`tailscale` CLI; the Kotlin/Compose Android app embeds `tsnet` and includes its
own SSH terminal and SFTP file manager.

## Table of contents

- [Highlights](#highlights)
- [Platforms](#platforms)
- [Android app](#android-app)
- [Concepts: server, panel, and your personal key](#concepts-server-panel-and-your-personal-key)
- [Two ways to fill the form](#two-ways-to-fill-the-form)
- [Build](#build)
- [Privileges (Linux)](#privileges-linux)
- [Run locally](#run-locally)
- [Configuration file](#configuration-file)
- [Security and reliability](#security-and-reliability)
- [Translations](#translations)
- [Validation](#validation)
- [Source layout](#source-layout)
- [License](#license)

---

## Highlights

- **One-click connect / disconnect** via `tailscale up` / `tailscale down`.
- **Live peer list** with online/offline status, IP, and short name; favourite
  peers float to the top.
- **SSH and SFTP launchers** open the system terminal and file manager
  pointing straight at an online peer (Linux: `xdg-open sftp://…`; Windows:
  WinSCP → sshfs-win → native `sftp://` fallback).
- **Sign in with a BigScale panel account**: BigLace asks the panel for a
  fresh pre-auth key and fills the form for you — no copy-pasting keys.
- **System tray indicator** (StatusNotifierItem on Linux, Shell_NotifyIcon on
  Windows) with quick connect/disconnect and a "minimise to tray" close
  button.
- **Desktop notifications** when peers come online or drop off, debounced so
  flapping peers don't spam the notification daemon.
- **Latency pings**, **self-update check**, and a **Headscale health badge**
  in the header — all running on background threads so the UI stays
  responsive.
- **Auto-reconnect with exponential backoff** when a session drops.
- **Start with the desktop session** so BigLace opens after login and can
  auto-connect without being launched manually.
- **Internationalised** via gettext (29 locales shipped, including pt-BR, en,
  es, de, fr, it, …).
- **Native credential storage** for panel passwords via the OS keyring
  (Secret Service on Linux, Windows Credential Manager, Apple Keychain).
- **Android client with embedded `tsnet`**, a VT-style SSH terminal, a full
  SFTP browser, foreground connectivity, and optional connect-on-boot.
- **Concurrent Android sessions**: terminals and file browsers remain connected
  in independent per-device tabs; `+` opens a manual session and closing a tab
  disconnects only that SSH/SFTP client.
- **Hardened secrets and SSH trust**: desktop enrollment keys use the OS
  keyring; Android secrets use Keystore-backed AES-GCM and host keys use TOFU.

---

## Platforms

| Platform | Status | Distribution |
|---|---|---|
| **Linux** (Arch / BigCommunity, Fedora, Debian — anywhere with GTK4 + libadwaita 1.4) | Primary target. | `pkgbuild/PKGBUILD` ships `/usr/bin/biglace`, the `.desktop` entry, icons, and compiled `.mo` catalogues. |
| **Windows 10 / 11 (x64)** | Supported. | `.github/workflows/windows.yml` produces a single signed-ready MSI (`biglace-<version>-x86_64.msi`) bundling biglace.exe + the gvsbuild GTK4 runtime. |
| **Android 8+ (API 26, ARM64)** | Native app, current mobile version `0.9.1`. | Debug-signed test APKs live in `mobile/dist/`; Gradle also builds ARMv7, x86, and x86_64 splits. |
| **macOS** | Code compiles (config path, keyring, and `Command::new` calls are cfg-gated), but no installer is shipped yet. | Build from source with `cargo build --release`. |

The desktop codebase is one Rust crate; platform differences are gated behind
`#[cfg(target_os = …)]` blocks. Android lives under `mobile/` and combines a
Kotlin/Compose application with a small Go bridge compiled to an AAR.

---

## Android app

The Android client joins the tailnet inside the app through `tsnet`. It does
not require a separately installed Tailscale app and only forwards BigLace's
own SSH/SFTP sockets; it is not a device-wide `VpnService`.

Current mobile features:

- Real peer list, online state, favorites, per-peer SSH login overrides, panel
  enrollment, foreground connection status, auto-connect, and connect-on-boot.
- Embedded SSH terminal with UTF-8 handling, VT screen state, resize-aware PTY,
  pinch zoom, extra keys, key/password/keyboard-interactive authentication, and
  automatic fallback.
- Independent terminal tabs, so several peers can remain connected at once.
- SFTP browsing with independent per-peer tabs, search, name/size/type sorting,
  upload, download/open/share, rename, move, recursive delete, and new folders.
- Generated SSH keys, encrypted per-host passwords and usernames, and TOFU host
  fingerprints with an explicit reset action for rotated server keys.
- Android Keystore-backed encryption, bounded temporary files and HTTP bodies,
  strict panel URL validation, and cancellation-safe service/bridge lifecycle.
- Adaptive launcher/monochrome icon and a dedicated six-node notification icon.

The current ARM64 test build is
[`mobile/dist/biglace-mobile-0.9.1-vc28-debug.apk`](mobile/dist/biglace-mobile-0.9.1-vc28-debug.apk).
It is debug-signed for testing, not a store/release signing configuration.

---

## Concepts: server, panel, and your personal key

BigScale has three things that are easy to mix up. Read this once and the
fields in the UI will make sense.

### 1. The server (BigScale / Headscale)

This is the coordinator that the VPN clients connect to. It speaks the
Tailscale protocol. In the BigLace UI it is the **"Server URL"** field.

- In production, this is usually a public address such as
  `https://bigscale.example.com`.
- In a local development setup with the project's `docker-compose.override.yml`
  it is `http://localhost:18080`.
- BigLace requires this field for manual setup. An empty server is not treated
  as an implicit "official Tailscale" mode.

The server has its own administrative API key (it shows up in the panel as
`hskey-api-…` and is **truncated** there on purpose). You **never** put that
key into BigLace. It exists only so the panel itself can talk to the
Headscale daemon.

### 2. The panel (BigScale web UI)

This is the SvelteKit website where an administrator manages users and keys.
In a local development setup it lives at `http://localhost:3000`. In
production it usually lives at the same domain as the server (or a sibling
subdomain).

You only need the panel address inside BigLace if you use the
**"Sign in with panel account"** menu, described below.

### 3. Your personal pre-auth key

This is the one credential that an end user actually needs. It is **not** the
server's key — it is a per-person key generated by an administrator inside
the panel.

In Headscale, "users" are just labels (namespaces) used to group devices.
They have no password. To let a person join the mesh, the admin:

1. Opens the panel → **Users**.
2. Creates (or picks) a user, e.g. `alice`.
3. Clicks **"New key"**, picks options (reusable / ephemeral / expiry) and
   confirms.
4. The panel reveals the generated key **once**. The admin copies it and
   sends it to that person over a secure channel.

That key is what goes into BigLace's **"Pre-auth key (yours)"** field, under
the **"Your identity on the network"** section. Each person has their own
key. Multiple devices belonging to the same person can reuse one key (if
the admin marked it reusable) or each get their own.

### Quick mental model

| You see in panel | What it is | Goes in BigLace? |
|---|---|---|
| `hskey-api-…` (truncated) | Server admin API key | **No, never.** |
| Long string from "New key" modal | Pre-auth key for one user | **Yes — "Pre-auth key (yours)" field.** |
| Panel admin login | Site login for the administrator | Only in the "Sign in with panel account" menu. |

---

## Two ways to fill the form

### A. Manual

Best for end users who received a key from the admin.

- **Server URL**: required. E.g. `https://bigscale.example.com` (production) or
  `http://localhost:18080` (local dev with the override).
- **Pre-auth key (yours)**: the key the admin sent you.
- **Device name**: a lowercase DNS-safe label, e.g. `alice-laptop`. The clients
  normalize spaces, accents, and unsupported characters to hyphens.

Click **Save**, then **Connect**.

The official Tailscale control plane is a different flow: it normally uses
Tailscale's own login/default server and requires a Tailscale account or an
auth key generated in that tailnet. BigLace does not infer that mode from an
empty server field; the coordinator must be explicit.

### B. "Sign in with panel account"

Convenience shortcut for an administrator (or anyone with panel
credentials) on their own machine. From the menu:

- **Panel URL**: the admin website, e.g. `http://localhost:3000`.
- **Username / password**: the panel login.
- **Node user**: the Headscale user this device belongs to.

BigLace calls `POST /api/v1/preauth-key` on the panel; the panel verifies
the admin login, makes sure the user exists, generates a fresh 1-hour
pre-auth key, and returns it together with the canonical server URL. The
main form is filled in automatically and the config is saved.

This menu is **not** something every end user has to use. Most end users
will never log into the panel — they just paste the key the admin gave
them.

---

## Build

### Linux

Runtime dependencies: `gtk4`, `libadwaita`, `tailscale`, `openssl`.
Build dependencies: `rust`, `cargo`, `pkgconf`, `openssl` (for the
`native-tls` backend used to talk to the panel HTTPS API).

```bash
cargo build --release
# binary: target/release/biglace
```

To package on Arch / BigCommunity:

```bash
cd pkgbuild
makepkg -si
```

The `PKGBUILD` installs:

| Path | Source |
|---|---|
| `/usr/bin/biglace` | Rust binary |
| `/usr/share/applications/org.communitybig.biglace.desktop` | `usr/share/applications/org.communitybig.biglace.desktop` |
| `/usr/share/icons/hicolor/scalable/apps/org.communitybig.biglace.svg` | `usr/share/icons/hicolor/scalable/apps/org.communitybig.biglace.svg` |
| `/usr/share/icons/hicolor/symbolic/apps/org.communitybig.biglace-symbolic.svg` | `usr/share/icons/hicolor/symbolic/apps/org.communitybig.biglace-symbolic.svg` |
| `/usr/share/locale/<lang>/LC_MESSAGES/biglace.mo` | `usr/share/locale/<lang>/LC_MESSAGES/biglace.mo` |
| `/usr/share/licenses/biglace/LICENSE` | `LICENSE` |
| `/usr/share/doc/biglace/README.md` | this file |

The repository's `usr/` tree is a literal mirror of the install layout: the
PKGBUILD just copies `usr/` straight into `${pkgdir}/`. Pre-compiled message
catalogues (`.mo`) live under `usr/share/locale/<lang>/LC_MESSAGES/biglace.mo`
and are produced by the project's external translation pipeline from the
`.po` files in `locale/` — neither the PKGBUILD nor `cargo build` regenerates
them.

### Android

Requirements: JDK 17, Android SDK/platform 36, and an Android NDK. The committed
`tsbridge.aar` is sufficient for normal app builds:

```bash
cd mobile/android
./gradlew testDebugUnitTest lintDebug assembleDebug
```

Gradle creates ABI-specific APKs under
`mobile/android/app/build/outputs/apk/debug/`. The ARM64 output is
`app-arm64-v8a-debug.apk`. Release builds enable R8/resource shrinking but are
unsigned until a signing configuration is supplied:

```bash
./gradlew assembleRelease
```

Rebuild the embedded Go/`tsnet` AAR after changing `mobile/tsbridge`:

```bash
cd mobile/tsbridge
./build-aar.sh
```

The script builds `libgojni.so` for ARM64, ARMv7, x86, and x86_64, strips the
native binaries, and requests 16 KiB ELF page alignment. It discovers the SDK
from `ANDROID_HOME`, `ANDROID_SDK_ROOT`, or Android's `local.properties` and
uses the newest installed NDK unless `ANDROID_NDK_HOME` is set.

### Windows (MSI)

End-user installer is built by GitHub Actions:

- **Trigger:** the `build-package` action publishes a release, then sends a
  `windows-msi-build` repository dispatch to this repo. You can also run the
  **Windows MSI** workflow manually from the Actions tab.
- **Output:** `biglace-<version>-x86_64.msi` published as a workflow artifact
  and, when a tag is provided, attached to that GitHub Release.
- **Install scope:** per-machine, into `C:\Program Files\BigLace`. Adds a
  Start Menu shortcut and uninstalls cleanly through *Settings → Apps*.

The pipeline pins `gvsbuild` for a reproducible MSVC GTK4 + libadwaita SDK,
bundles all GTK runtime DLLs alongside `biglace.exe`, harvests them with
WiX `heat.exe`, and produces the MSI through `cargo wix`. See
[`.github/workflows/windows.yml`](.github/workflows/windows.yml) and
[`wix/main.wxs`](wix/main.wxs) for the full recipe.

To test the MSI path without waiting for the full `build-package` flow, run the
**Windows MSI** workflow manually against a branch or tag. To test it locally,
use a Windows VM/host with WiX 3, Rust MSVC, ImageMagick, and gvsbuild installed:

```powershell
cargo build --release --no-default-features --target x86_64-pc-windows-msvc
cargo install cargo-wix --locked --version 0.3.9
heat.exe dir target\wix-stage -cg RuntimeFiles -dr APPLICATIONFOLDER -ag -srd -sfrag -var var.StageDir -out wix\runtime.wxs
cargo wix --no-build --nocapture --target x86_64-pc-windows-msvc -C "-ext" -C "WixUtilExtension" -C "-dStageDir=target\wix-stage"
```

`--no-default-features` disables jemalloc, which has no Windows backend.

---

## Privileges (Linux)

The `tailscaled` daemon only accepts `up`/`down` from `root` or from the
user marked as the tailscale operator. If neither applies, BigLace falls
back automatically to running the command via `pkexec`, so the user is
prompted by polkit and the command goes through.

To stop seeing the polkit prompt every connect/disconnect, use the menu
**"Make this user the tailscale operator"** once. It runs
`pkexec tailscale set --operator=$USER`. After that, BigLace can drive
`tailscale up`/`down` as your user with no elevation.

This is equivalent to the message that the tailscale CLI itself prints:
*"To not require root, use 'sudo tailscale set --operator=$USER' once."*

On Windows, the Tailscale service runs as `LocalSystem` and BigLace talks to
it through the standard CLI, which is already permitted for any local user —
no equivalent polkit dance is needed.

---

## Run locally

```bash
cargo run --release
```

If you run a local BigScale stack with the override file, point BigLace at:

- **Server URL**: `http://localhost:18080`
- **Pre-auth key**: generated in the panel at `http://localhost:3000` →
  Users → expand a user → "New key".
- **Device name**: anything.

To exercise the panel-login path, use the menu **"Sign in with panel
account"** with `http://localhost:3000` and your panel admin credentials.

---

## Configuration file

BigLace persists settings to a per-user TOML file. The location follows
each platform's convention:

| Platform | Path |
|---|---|
| Linux | `~/.config/biglace/config.toml` |
| Windows | `%APPDATA%\biglace\config.toml` |
| macOS | `~/Library/Application Support/biglace/config.toml` |

```toml
server_url   = "https://bigscale.example.com"
hostname     = "alice-laptop"
auto_connect = false
enrolled     = true
panel_url    = "https://bigscale.example.com"   # optional, used by the panel-login menu
panel_username = "alice"
```

You may edit non-secret settings directly or use the UI. Panel passwords and
enrollment/pre-auth keys are stored in the OS keyring and are never serialized
to TOML. Existing plaintext `authkey` fields are migrated when a usable server
URL is available. The durable `enrolled` marker records that the local
Tailscale state is registered after the disposable enrollment key is cleared.

---

## Security and reliability

- Desktop config writes use a same-directory temporary file, file and parent
  directory sync, and atomic rename. Invalid config is preserved as a backup;
  BigLace refuses to overwrite data it could not read or back up.
- Desktop panel/update HTTP uses native TLS, redirect blocking, response-size
  limits, and deadlines. Remote credential endpoints require HTTPS; loopback
  HTTP remains available for local development.
- Tailscale CLI workers have timeouts and bounded output, status refresh is
  single-flight with a short cache, and reconnect retries use exponential
  backoff until success or an explicit disconnect.
- Android secrets use an AES-GCM key held by Android Keystore. Existing
  plaintext preferences migrate transparently; corrupt encrypted values are
  discarded rather than returned as credentials.
- Android SSH verifies host fingerprints with trust-on-first-use. Concurrent
  terminal writes are serialized, UTF-8 is decoded incrementally, and SFTP
  operations are mutex-protected with bounded recursive deletion and isolated
  temporary staging directories.
- Panel clients reject malformed authorities/userinfo, cap response bodies,
  disable redirects, and allow plaintext HTTP only for loopback or explicit
  tailnet endpoints where the source-IP-authenticated panel API requires it.
- The Go bridge makes startup cancellable, handles stop/start races, bounds
  status/log work, and closes one-shot forwarding listeners after the SSH
  transport connects.

---

## Translations

Translatable strings live in `locale/*.po`. The list of locale codes
shipped with the package is in `locale/LINGUAS`, and the source files
scanned for translatable strings are listed in `locale/POTFILES.in`.

To add a language:

1. Add the code (e.g. `fr`, `it`) to `locale/LINGUAS`.
2. Copy `locale/biglace.pot` to `locale/<lang>.po` and translate the
   `msgstr` entries. Use the POSIX form with an underscore for regional
   variants — `pt_BR`, not `pt-BR`.

Compiled `.mo` files are committed under
`usr/share/locale/<lang>/LC_MESSAGES/biglace.mo` and are produced by the
project's external translation pipeline (which also re-extracts
`biglace.pot` via `xgettext` with the `tr` and `trf` keywords). Neither
the PKGBUILD nor `cargo build` regenerates them.

For development, with locally produced `.mo` files, set
`BIGLACE_LOCALEDIR=/path/to/locale` to make the binary use that directory
instead of `/usr/share/locale`.

---

## Validation

Run the desktop checks from the repository root:

```bash
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo audit --deny warnings
```

Run Android and Go checks with:

```bash
cd mobile/android
./gradlew testDebugUnitTest lintDebug assembleDebug

cd ../tsbridge
go test -race ./...
go mod verify
```

Packaging sources can additionally be checked with `shellcheck`, `namcap`,
`desktop-file-validate`, `xmllint`, and `msgfmt -c`. Android APKs/AARs are built
as per-ABI artifacts; release candidates should be checked with Android
Build-Tools `zipalign -c -P 16 4` before signing.

---

## Source layout

```
biglace/
├── Cargo.toml
├── mobile/
│   ├── android/                            # Kotlin + Compose Android app
│   │   └── app/src/
│   │       ├── main/java/org/communitybig/biglace/
│   │       │   ├── core/                   # data, mesh, panel, SSH sessions
│   │       │   ├── feature/                # peers, terminal, files, settings
│   │       │   ├── service/                # foreground mesh + boot receiver
│   │       │   └── ui/                     # navigation, session tabs, theme
│   │       └── test/                       # JVM unit tests
│   ├── tsbridge/                           # Go tsnet bridge + AAR build script
│   └── dist/                               # versioned test APK + SHA-256
├── usr/                                  # literal mirror of /usr at install time
│   └── share/
│       ├── applications/
│       │   └── org.communitybig.biglace.desktop
│       ├── icons/hicolor/
│       │   ├── scalable/apps/org.communitybig.biglace.svg
│       │   └── symbolic/apps/org.communitybig.biglace-symbolic.svg
│       └── locale/<lang>/LC_MESSAGES/biglace.mo   # one per language in LINGUAS
├── locale/                               # translation sources (29 languages)
│   ├── POTFILES.in
│   ├── LINGUAS
│   ├── biglace.pot
│   └── <lang>.po
├── pkgbuild/                             # Arch / BigCommunity packaging
│   ├── PKGBUILD
│   └── pkgbuild.install
├── wix/                                  # Windows MSI assets
│   ├── main.wxs                          # WiX 3 installer template
│   └── License.rtf                       # EULA dialog text
├── .github/workflows/
│   └── windows.yml                       # Windows MSI CI pipeline
└── src/
    ├── main.rs        # entry point + i18n init + jemalloc (Linux/macOS)
    ├── i18n.rs        # gettext bootstrap, tr! / trf! macros
    ├── config.rs      # per-OS config.toml location + (de)serialization
    ├── secrets.rs     # OS keyring wrapper (Secret Service / Credential Manager / Keychain)
    ├── tailscale.rs   # tailscale CLI wrappers, peer parsing, SSH/SFTP launchers
    ├── panel.rs       # BigScale panel HTTP client
    ├── tray.rs        # cross-platform tray facade (Command / Handle / spawn)
    ├── tray/
    │   ├── linux.rs   # ksni / StatusNotifierItem backend
    │   ├── windows.rs # tray-icon backend with a Win32 message pump
    │   └── stub.rs    # no-op fallback for unsupported targets
    └── window/
        ├── mod.rs     # main window, signal wiring, background workers
        ├── content.rs # right-pane peer list view
        ├── sidebar.rs # left-pane connection form
        ├── peer_row.rs# per-peer expander row widget
        ├── dialogs.rs # About + panel-login modals
        └── style.rs   # CSS overrides
```

---

## License

MIT — see [`LICENSE`](LICENSE).
