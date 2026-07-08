# BigLace Mobile (Android) — planning docs

> **Status: planning only.** Nothing in this folder is implemented yet. These
> documents capture the product vision, architecture decisions, and roadmap for
> an Android companion to the BigLace desktop client. The Android project
> itself will live in `mobile/android/` once implementation starts.

## Vision

BigLace Mobile brings the BigLace experience to Android:

1. **Join the BigScale / Headscale mesh** from a phone or tablet — same
   "paste a server URL + pre-auth key" flow (or panel sign-in) as the desktop
   app.
2. **A beautiful terminal** (Termius-style): SSH into any online peer with a
   polished, themeable terminal — color schemes, ligature fonts, an extra-keys
   row, multiple tabs.
3. **A beautiful file manager**: browse any online peer's folders over SFTP —
   breadcrumbs, thumbnails, upload/download with progress, share into/out of
   Android.

The desktop app shells out to the `tailscale` CLI and opens the *system*
terminal and file manager. Android has neither, so the mobile app must bring
its own: an embedded mesh layer, an embedded SSH terminal, and an embedded
SFTP browser. That is the core difference — and most of the work.

## Feature parity map

| Desktop (biglace)                          | Mobile equivalent                                    |
|--------------------------------------------|------------------------------------------------------|
| `tailscale up/down` via CLI                | Embedded Tailscale (libtailscale AAR + `VpnService`) |
| Peer list from `tailscale status --json`   | Peer list from embedded client's LocalAPI            |
| Open system terminal → `ssh user@host`     | Built-in SSH terminal (Termius-style)                |
| Open system file manager → `sftp://…`      | Built-in SFTP file manager                           |
| Panel sign-in (`POST /api/v1/preauth-key`) | Same HTTP API, same request/response                 |
| `os_user` propagation to panel             | Same endpoints over the tunnel                       |
| System tray + notifications               | Foreground-service notification + peer notifications |
| OS keyring for panel password              | Android Keystore + EncryptedSharedPreferences        |
| Autostart with session                     | Always-on VPN / boot receiver (optional)             |
| gettext `.po` (29 locales)                 | `strings.xml`, converted from the same `.po` files   |

## Documents

| File | Contents |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Layers, the three mesh-integration options (A/B/C) and the chosen one, module layout, data model, panel API contract, security model |
| [TECH-STACK.md](TECH-STACK.md) | Chosen libraries with licenses, and the alternatives considered |
| [UI-DESIGN.md](UI-DESIGN.md) | Screen inventory, wireframes, terminal & file-manager design, theming |
| [ROADMAP.md](ROADMAP.md) | Milestones M0–M5 with acceptance criteria |

## Implementation

The Android project lives in **[android/](android/)** — see
[android/README.md](android/README.md) for build instructions and the
per-milestone status. The current cut (M0 + M1 foundation) is a buildable,
runnable app: the full Compose UI shell, the `MeshBackend` seam with a fake
backend, the peers list with persisted favorites, the settings/connection form,
and a `PanelClient` implementing the exact desktop panel contract. Terminal
(M2), the SFTP file manager (M3), and the embedded mesh (M4) are scaffolded
behind their interfaces.

## Key decisions (summary)

- **Kotlin + Jetpack Compose + Material 3**, MVVM, coroutines. No cross-platform
  framework: the terminal and the VPN service are deeply platform-specific, and
  Compose gives the "beautiful" bar the product asks for.
- **Mesh layer**: MVP ships as a **companion app** (relies on the official
  Tailscale Android app pointed at the BigScale/Headscale server) to de-risk;
  v1.0 embeds **libtailscale** (BSD-3-Clause, the same library the official
  client uses) behind our own `VpnService` for true one-click connect. See
  ARCHITECTURE.md § "Mesh integration options".
- **SSH + SFTP**: one library for both — **sshj** (Apache-2.0) — so the
  terminal and the file manager share connections, host-key verification, and
  key management.
- **Terminal emulation**: license decides. The best Android emulator
  (Termux's `terminal-view`) is GPLv3; adopting it means the mobile app ships
  under GPLv3 (kept possible by making `mobile/` a separately-licensed
  sub-project or its own repo). The Apache-2.0 fallback is ConnectBot's
  emulation stack. Decision needed before M2 — see TECH-STACK.md § "Terminal
  emulator".
- **Repository**: start in this repo under `mobile/` for shared docs/locale
  tooling; split into `biglace-mobile` later if the GPLv3 route is taken or CI
  weight demands it.

## What stays identical to desktop

These behaviors are contracts the mobile app must honor so both clients feel
like one product (all implemented today in `src/` of this repo):

- **Panel sign-in**: `POST {panel}/api/v1/preauth-key` with
  `{username, password, node_user, hostname}` → `{authkey, server_url}`
  (`src/panel.rs`).
- **Panel discovery over the tailnet**: the panel is the peer whose DNS name is
  `panel.<MagicDNSSuffix>`; peer-facing endpoints are reached at
  `http://<panel-peer-ip>:3000` and authenticate by tailnet source IP
  (`src/panel.rs`, `src/tailscale.rs::panel_peer_ip`).
- **`os_user` propagation**: `POST /api/devices/me/os-user` after connect;
  `GET /api/devices/os-users` to learn the right SSH login per peer, falling
  back to the `tag:user-<name>` ACL tag, then to the peer's hostname
  (`src/tailscale.rs`).
- **Display-name precedence**: first DNS label → hostname → IP, so renames done
  in the panel surface immediately (`Peer::display_name`).
- **No implicit official-Tailscale mode**: the coordinator URL is always
  explicit.
