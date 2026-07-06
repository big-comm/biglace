# BigLace Mobile — Architecture

Planning document. Nothing here is implemented yet.

## 1. Big picture

```
┌──────────────────────────────────────────────────────────────────┐
│                        UI (Jetpack Compose)                      │
│  Peers screen · Terminal screen · Files screen · Settings/Panel  │
├──────────────────────────────────────────────────────────────────┤
│                     ViewModels (MVVM, StateFlow)                 │
├───────────────┬──────────────────┬───────────────┬───────────────┤
│  MeshManager  │   SshSessionMgr  │  SftpManager  │  PanelClient  │
│ (connect/down │ (sshj: channels, │ (sshj SFTP:   │ (preauth-key, │
│  peers, DNS)  │  PTY, keepalive) │  ls/get/put)  │  os-users)    │
├───────────────┴──────────────────┴───────────────┴───────────────┤
│        Persistence: Room (hosts, favorites, known_hosts),        │
│        DataStore (prefs), Keystore/EncryptedPrefs (secrets)      │
├──────────────────────────────────────────────────────────────────┤
│   Mesh layer (one of options A/B/C below) — provides: connect,   │
│   disconnect, peer list w/ online state, tailnet DNS suffix,     │
│   sockets that reach 100.64/10 addresses                         │
└──────────────────────────────────────────────────────────────────┘
```

Everything above the mesh layer is identical across options A/B/C, so the mesh
layer must sit behind a single interface from day one:

```kotlin
interface MeshBackend {
    val state: StateFlow<MeshState>            // Disconnected / Connecting / Up(selfNode)
    val peers: StateFlow<List<Peer>>
    suspend fun connect(server: String, authKey: String?, hostname: String)
    suspend fun disconnect()
    /** Socket factory whose connections are routed through the tailnet. */
    fun socketFactory(): javax.net.SocketFactory
}
```

## 2. Mesh integration options

Android has no `tailscale` CLI and no system daemon, so the desktop approach
(shell out to the CLI, parse `status --json`) does not translate. Three viable
designs, in increasing order of effort:

### Option B — Companion app (MVP)

The **official Tailscale Android app** handles the VPN. It supports custom
coordination servers (Headscale/BigScale) via its alternate-server login flow.
BigLace Mobile then:

- never touches `VpnService`; every TCP socket it opens already routes through
  the tunnel because the Tailscale app owns the device VPN;
- gets the peer list from the **panel** (`GET /api/devices/…` over the tunnel)
  and/or the Headscale API, not from a LocalAPI (the official app exposes none
  to third parties);
- deep-links the user to the Tailscale app for connect/disconnect.

| Pros | Cons |
|---|---|
| Zero Go toolchain; pure Kotlin; fastest to ship | No one-click connect inside our app |
| VPN edge cases (MTU, DNS, doze) are Tailscale's problem | Peer online-state depends on panel/Headscale, may lag |
| | Two apps to install; setup friction for end users |

### Option A — Embedded full client (v1.0 target)

Embed **libtailscale** — the gomobile-built AAR from the open-source
`tailscale-android` project (BSD-3-Clause), the exact engine the official app
uses — and implement our own `VpnService`:

- `IPNService` extends `android.net.VpnService`; libtailscale drives it and
  hands us a tun fd;
- LocalAPI available in-process → peer list, health, MagicDNS suffix — the
  same JSON shapes `src/tailscale.rs` already parses on desktop;
- login via `--login-server` equivalent (control URL + pre-auth key), exactly
  mirroring desktop `try_connect()`: reset prefs, set control URL, auth key,
  hostname, accept-routes;
- a **foreground service notification** is the mobile "tray icon":
  connect/disconnect action buttons, status line.

| Pros | Cons |
|---|---|
| Full biglace parity: one app, one-click connect | Go/gomobile in the build (pin version, prebuild AAR in CI) |
| Peer state straight from the engine (same JSON as desktop) | We own VPN lifecycle bugs (battery, doze, always-on) |
| Whole device joins the mesh, not just our app | Android allows **one active VpnService** — activating ours kicks out any other VPN (incl. official Tailscale app) |

### Option C — Embedded userspace mesh, no VPN (`tsnet`/netstack)

Wrap Tailscale's `tsnet` (userspace netstack) with a small Go shim exposed via
gomobile: the app joins the mesh **as a node**, but only sockets opened
*inside the app* reach the tailnet. No `VpnService`, no VPN permission.

Interesting because SSH + SFTP are the app's only network consumers — device-
wide VPN is not strictly needed for the product to work. Coexists with any
other VPN. Costs: custom Go shim to maintain; the rest of the device never
joins the mesh; background keepalive drains battery if not managed.

### Decision

- **MVP (M1)**: Option **B** — proves the terminal + file manager (the bulk of
  the product) with zero Go.
- **v1.0 (M4)**: Option **A** behind the same `MeshBackend` interface.
- Option **C** stays documented as the fallback if coexistence with other VPNs
  becomes a real user demand.

## 3. Module layout (Gradle, once implementation starts)

```
mobile/android/
├── app/                  # Compose UI, navigation, DI wiring (Hilt)
├── core/
│   ├── mesh-api/         # MeshBackend interface + Peer/Status models
│   ├── mesh-companion/   # Option B impl (panel-driven peer list)
│   ├── mesh-embedded/    # Option A impl (libtailscale + IPNService)  [M4]
│   ├── ssh/              # sshj wrapper: session pool, PTY, keepalive, host keys
│   ├── sftp/             # SFTP ops, transfer queue (WorkManager)
│   ├── panel/            # PanelClient (preauth-key, os-user endpoints)
│   └── data/             # Room, DataStore, Keystore-backed secret store
├── feature/
│   ├── peers/            # peer list + detail (mirrors desktop content pane)
│   ├── terminal/         # terminal screen, tabs, extra-keys row, themes
│   ├── files/            # SFTP browser, transfers UI, DocumentsProvider
│   └── settings/         # server form, panel sign-in, appearance
└── build-logic/          # convention plugins
```

## 4. Data model (mirrors `src/tailscale.rs`)

```kotlin
data class Peer(
    val hostname: String,        // OS hostname reported by the peer
    val ipv4: String?,           // first 100.64/10 address
    val ipv6: String?,
    val dnsName: String,         // headscale-assigned FQDN
    val online: Boolean,
    val os: String,
    val owner: String,           // BigScale account (LoginName via UserID)
    val sshUser: String?,        // from panel os-users map, else tag:user-<x>
    val lastSeen: Instant?,
    val tags: List<String>,      // ACL tags, "tag:" prefix stripped
    val exitNodeOffered: Boolean,
    val exitNodeActive: Boolean,
) {
    /** Same precedence as desktop Peer::display_name(). */
    val displayName: String
        get() = dnsName.trimEnd('.').substringBefore('.').ifEmpty { null }
            ?: hostname.ifEmpty { null } ?: (ipv4 ?: ipv6 ?: "")
}
```

Rules carried over from desktop (do not re-derive; they encode fixes already
made there):

- **Favorites / overrides / latency caches key on `hostname`**, never on
  `displayName` (renames in the panel must not lose user data).
- **SSH login resolution order**: panel `os-users` map → `tag:user-<name>` →
  peer hostname (`ssh <hostname>@…` beats `ssh <local-user>@…` on Linux
  peers).
- **Target picking for SSH/SFTP**: prefer MagicDNS name when the tailnet
  reports a DNS suffix, fall back to the IPv4 (desktop `pick_target`). On
  Android + Option B, prefer the IP: MagicDNS resolution depends on the
  official app's DNS config, which we don't control.

## 5. Panel API contract (source of truth: `src/panel.rs`)

| Call | When | Auth |
|---|---|---|
| `POST {panelUrl}/api/v1/preauth-key` body `{username, password, node_user, hostname}` → `{authkey, server_url}` | "Sign in with panel account" | body credentials |
| `POST http://<panel-peer-ip>:3000/api/devices/me/os-user` body `{os_user}` | after connect; idempotent (treat 200 and 304 as success) | tailnet source IP |
| `GET  http://<panel-peer-ip>:3000/api/devices/os-users` → `{hostname: os_user}` | on peer-list refresh | tailnet source IP |

- Panel peer discovery: the peer whose `DNSName` first label is `panel` under
  the current `MagicDNSSuffix`. Non-BigScale tailnets have no such peer —
  **degrade silently** (empty map, skip the POST), exactly like desktop.
- Treat `401`/`404` on `os-users` as "older panel" → empty map, no error UI.
- `os_user` on Android: there is no OS login; send the device owner's chosen
  profile name (settings field, default = hostname).

## 6. SSH & terminal architecture

- **One `SshConnection` per peer**, multiplexed: the terminal opens `shell`
  channels, the file manager opens `sftp` channels over the *same* transport
  (sshj supports this natively). Kill the transport → both die; reconnect
  logic lives in one place.
- **Auth order**: ed25519 key from Keystore-encrypted storage → password
  prompt (never stored unless the user opts in). Key generation and export of
  the public key (`copy to clipboard`, `share`) in Settings — the user pastes
  it into `~/.ssh/authorized_keys` on peers, or a future panel feature
  distributes it.
- **Host keys**: TOFU with a Room-backed `known_hosts`; on mismatch show a
  full-screen warning (Termius-style) with old/new fingerprints; never
  auto-accept a changed key.
- **Sessions survive rotation and brief background**: sessions live in a
  foreground service (`connectedDeviceType=dataSync`-style) while at least one
  terminal tab or transfer is active; Android will kill background sockets
  otherwise.
- **PTY**: request `xterm-256color`, propagate resize events from Compose
  layout changes, UTF-8 always.

## 7. SFTP file-manager architecture

- Directory listing = suspend calls on the shared connection; cache the last
  listing per (peer, path) for instant back-navigation.
- **Transfers** go through a queue implemented on WorkManager: survive app
  death, show progress notifications, resume-on-Wi-Fi option. Foreground
  service for active transfers.
- Downloads land in the app cache for "open with…" (via `FileProvider`) or in
  `Downloads/BigLace/` for explicit "save".
- Uploads accept Android share-sheet intents (`ACTION_SEND`,
  `ACTION_SEND_MULTIPLE`) — "share a photo to a peer" is a headline flow.
- **Stretch goal (M5)**: a `DocumentsProvider` exposing online peers as roots,
  so peers' folders appear inside the system Files app and any document picker
  — the mobile analog of desktop's GVfs `sftp://` integration.

## 8. Security model

| Secret | Storage |
|---|---|
| Pre-auth key | EncryptedSharedPreferences (Keystore master key) — desktop stores it in plaintext TOML; mobile can do better at no cost |
| Panel password | Only if user opts into "remember": EncryptedSharedPreferences (mirrors desktop's OS-keyring choice) |
| SSH private keys | Generated non-exportable in Keystore when possible; otherwise encrypted-at-rest with a Keystore key. Optional biometric unlock per connection |
| Host keys (`known_hosts`) | Room, integrity is the point — no encryption needed |

- All panel HTTP over the tunnel is plain HTTP to a 100.64/10 address (same as
  desktop) — acceptable because WireGuard already encrypts it; document this
  so a future reviewer doesn't "fix" it to HTTPS and break the source-IP auth.
- `preauth-key` sign-in goes to the public panel URL → must be HTTPS in
  production; warn (but allow) plain `http://` for local dev, mirroring
  desktop behavior.
- No analytics/telemetry.

## 9. Background & battery policy

- Mesh **up** + screen off: rely on the mesh layer's own keepalive (options
  A/B handle this); do not hold a wake lock.
- Terminal tab open: foreground service + notification ("session on
  alice-server"), stop the service when the last tab closes.
- Peer online/offline notifications (desktop parity): only while the app or
  its service is alive — **no** periodic background polling; document that
  push-style notifications would need panel support (e.g. a future
  WebSocket/FCM bridge), not client hacks.

## 10. i18n

- Single source of truth stays `locale/*.po` (29 languages already
  translated). Add a small converter (`.po` → `values-<lang>/strings.xml`) to
  the existing external translation pipeline so mobile strings ride the same
  train. Android locale codes differ slightly (`pt-BR` → `values-pt-rBR`);
  the converter owns that mapping.
- New mobile-only strings get added to the same `.pot` extraction so
  translators see one catalog.
