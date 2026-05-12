<div align="center">

<img src="usr/share/icons/hicolor/scalable/apps/org.communitybig.biglace.svg" width="128" alt="BigLace">

# BigLace

**Mesh VPN client with a graphical interface for Headscale and BigScale.**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows-blue)](#platforms)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-orange?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![GTK4 · Libadwaita](https://img.shields.io/badge/GTK4-Libadwaita-blueviolet?logo=gnome&logoColor=white)](https://gtk.org)
[![Latest release](https://img.shields.io/github/v/release/big-comm/biglace?label=release&color=brightgreen)](https://github.com/big-comm/biglace/releases)

</div>

---

BigLace wraps the `tailscale` CLI in a polished GTK4 + libadwaita interface so
end users can join a **BigScale** mesh (or any plain Headscale server) in a
few clicks: paste a server URL and a personal pre-auth key, or sign in with a
BigScale panel account and the key is generated for you.

## Table of contents

- [Highlights](#highlights)
- [Platforms](#platforms)
- [Concepts: server, panel, and your personal key](#concepts-server-panel-and-your-personal-key)
- [Two ways to fill the form](#two-ways-to-fill-the-form)
- [Build](#build)
- [Privileges (Linux)](#privileges-linux)
- [Run locally](#run-locally)
- [Configuration file](#configuration-file)
- [Translations](#translations)
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
- **Internationalised** via gettext (29 locales shipped, including pt-BR, en,
  es, de, fr, it, …).
- **Native credential storage** — pre-auth keys live in the OS keyring
  (Secret Service on Linux, Windows Credential Manager, Apple Keychain).

---

## Platforms

| Platform | Status | Distribution |
|---|---|---|
| **Linux** (Arch / BigCommunity, Fedora, Debian — anywhere with GTK4 + libadwaita 1.4) | Primary target. | `pkgbuild/PKGBUILD` ships `/usr/bin/biglace`, the `.desktop` entry, icons, and compiled `.mo` catalogues. |
| **Windows 10 / 11 (x64)** | Supported. | `.github/workflows/windows.yml` produces a single signed-ready MSI (`biglace-<version>-x86_64.msi`) bundling biglace.exe + the gvsbuild GTK4 runtime. |
| **macOS** | Code compiles (config path, keyring, and `Command::new` calls are cfg-gated), but no installer is shipped yet. | Build from source with `cargo build --release`. |

The codebase is one Rust crate; platform differences are gated behind
`#[cfg(target_os = …)]` blocks. Linux binaries from `cargo build` are
unaffected by the Windows port.

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

- **Server URL**: e.g. `https://bigscale.example.com` (production) or
  `http://localhost:18080` (local dev with the override).
- **Pre-auth key (yours)**: the key the admin sent you.
- **Device name**: anything — what other peers will see, e.g. `alice-laptop`.

Click **Connect**.

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

### Windows (MSI)

End-user installer is built by GitHub Actions:

- **Trigger:** push a tag matching `v*` (e.g. `git tag v1.0.0 && git push --tags`),
  or run the **Windows MSI** workflow manually from the Actions tab.
- **Output:** `biglace-<version>-x86_64.msi` published as a workflow artifact
  and (on tag push) attached to the GitHub Release.
- **Install scope:** per-machine, into `C:\Program Files\BigLace`. Adds a
  Start Menu shortcut and uninstalls cleanly through *Settings → Apps*.

The pipeline pins `gvsbuild` for a reproducible MSVC GTK4 + libadwaita SDK,
bundles all GTK runtime DLLs alongside `biglace.exe`, harvests them with
WiX `heat.exe`, and produces the MSI through `cargo wix`. See
[`.github/workflows/windows.yml`](.github/workflows/windows.yml) and
[`wix/main.wxs`](wix/main.wxs) for the full recipe.

To experiment locally on a Windows host with WiX 3 + Rust MSVC + gvsbuild
installed:

```powershell
cargo build --release --no-default-features --target x86_64-pc-windows-msvc
cargo install cargo-wix --locked --version 0.3.9
cargo wix --no-build --target x86_64-pc-windows-msvc
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
authkey      = "..."   # the user's personal pre-auth key
hostname     = "alice-laptop"
auto_connect = false
panel_url    = "https://bigscale.example.com"   # optional, used by the panel-login menu
```

You may edit the file directly or use the UI; both are equivalent. Pre-auth
keys are also mirrored to the OS keyring (Secret Service / Windows Credential
Manager / Keychain) so they don't sit in plaintext on disk for users who
care about that.

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

## Source layout

```
biglace/
├── Cargo.toml
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
