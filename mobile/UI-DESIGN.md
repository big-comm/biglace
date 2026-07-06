# BigLace Mobile — UI design

Planning document. Material 3, dynamic color, dark-first (the terminal is the
soul of the app and terminals live in the dark).

## Navigation map

```
                    ┌────────────────────┐
                    │   Onboarding /     │  first run only
                    │   Server setup     │  (manual key OR panel sign-in)
                    └─────────┬──────────┘
                              ▼
┌─────────────────────────────────────────────────────────┐
│                     Main scaffold                       │
│  Bottom bar: [Peers]  [Terminal]  [Files]  [Settings]   │
└──────┬───────────────┬───────────────┬────────────┬─────┘
       ▼               ▼               ▼            ▼
   Peer list       Sessions hub     Remote        Settings
   + detail        (open tabs) ──►  browser       (server, panel,
   sheet           Terminal view    per peer      appearance, keys)
```

## 1. Peers screen (home — mirrors the desktop right pane)

```
┌──────────────────────────────────────┐
│  BigLace          ⏻ Connected ▾  ⋮  │   ⏻ chip = connect/disconnect
│  bigscale.example.com                │   + health badge (desktop parity)
├──────────────────────────────────────┤
│  ★ FAVORITES                         │
│  ● alice-server      100.64.0.3      │   ● green = online, hollow = offline
│     linux · tales    [>_] [🗀]       │   [>_] terminal  [🗀] files
│  ● media-box         100.64.0.7      │
├──────────────────────────────────────┤
│  ALL PEERS                           │
│  ○ old-laptop        last seen 2d    │   offline rows dim, actions disabled
│  ● phone-bob         100.64.0.12     │
└──────────────────────────────────────┘
```

- Row tap → bottom sheet: full details (IPs, DNS name, OS, owner, tags, last
  seen, exit-node badges), actions: Terminal, Files, Ping, Copy IP, Favorite,
  "Use as exit node" (option A only).
- Pull-to-refresh; peers stream-update while visible (no polling when app is
  backgrounded — see ARCHITECTURE.md § 9).
- Online/offline transitions trigger local notifications, debounced like
  desktop (flapping peers must not spam).

## 2. Terminal (the Termius bar)

```
┌──────────────────────────────────────┐
│ ⌂ │ alice-server ✕ │ media-box ✕ │ + │   tab strip, swipe to switch
├──────────────────────────────────────┤
│ tales@alice-server:~$ htop           │
│ ...                                  │   terminal grid
│                                      │   pinch-zoom = font size
│                                      │   long-press = select/copy
├──────────────────────────────────────┤
│ Esc  Tab  Ctrl  Alt  ←  ↓  ↑  →  ⋯  │   extra-keys row (sticky)
├──────────────────────────────────────┤
│ [        system keyboard            ]│
└──────────────────────────────────────┘
```

- **Tabs**: one per session; sessions keep running while the app is in the
  foreground service; tab shows a spinner while connecting and a badge on
  output-while-unfocused.
- **Extra-keys row**: configurable; `Ctrl`/`Alt` are latching modifiers; `⋯`
  opens a second row (F-keys, PgUp/PgDn, Home/End, `-`, `|`, `/`, `~`).
- **Selection & clipboard**: long-press starts selection with draggable
  handles; URL detection with tap-to-open.
- **Appearance**: per-host or global — color scheme (Dracula/Nord/Catppuccin/
  Gruvbox/Solarized), font (JetBrains Mono default), size, cursor style/blink,
  bell → vibrate.
- **Connection UX**: first connect shows host-key fingerprint sheet (TOFU);
  changed key = full-screen red warning, no one-tap accept.
- **Snippets** (M5): saved commands, long-press `+` to run one in a new tab.

## 3. Files (SFTP browser)

```
┌──────────────────────────────────────┐
│ alice-server  /home/tales/photos     │   breadcrumb, tap segment to jump
├──────────────────────────────────────┤
│ [🗀] ..                              │
│ [🗀] vacation-2026        jun 12     │
│ [🖼] IMG_0231.jpg   2.1 MB  [thumb]  │   thumbnails stream in lazily
│ [🖼] IMG_0232.jpg   1.9 MB  [thumb]  │
│ [📄] notes.md        4 KB            │
├──────────────────────────────────────┤
│              [＋ upload]  [⇅ sort]   │
└──────────────────────────────────────┘
```

- Tap file → preview when possible (image/text/video-stream later), else
  action sheet: Download, Open with…, Share, Rename, Delete, Permissions.
- Long-press → multi-select → bulk download/delete/move.
- Transfers bottom sheet: queue with per-item progress, pause/cancel; survives
  app death (WorkManager) with a progress notification.
- Receive-from-share-sheet: pick peer → pick folder → upload.
- Empty/error states designed: offline peer, permission denied, broken
  symlink.

## 4. Settings

- **Connection** (desktop sidebar parity): server URL, pre-auth key (masked,
  paste button), device name, auto-connect toggle.
- **Panel sign-in** (desktop dialog parity): panel URL, username, password,
  node user → fills the connection form via `preauth-key`.
- **Identity**: SSH key management — generate ed25519, view/copy/share public
  key, optional biometric unlock.
- **Appearance**: app theme (system/light/dark/AMOLED), terminal defaults.
- **About**: version, licenses screen (OSS notices — required by several deps).

## 5. Design language

- Material 3 with dynamic color; the accent falls back to BigLace brand color
  from the desktop icon when Monet is unavailable.
- Terminal and file screens support true-black (`#000`) AMOLED background.
- Motion: container-transform from peer row → terminal/files; standard
  durations, no gratuitous animation.
- Accessibility: TalkBack labels on every actionable row (peer state read as
  "online/offline"), min touch targets 48dp, terminal font scales with system
  font scale unless user pins it.
