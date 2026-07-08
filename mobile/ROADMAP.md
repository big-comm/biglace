# BigLace Mobile — Roadmap

Planning document. Milestones are cumulative; each ends in something
installable and demoable. Estimates assume one developer, part-time, and will
be wrong — treat them as relative sizes.

## M0 — Skeleton & decisions (small)

- [ ] Create `mobile/android/` Gradle project (Compose, Hilt, version
      catalogs, module layout from ARCHITECTURE.md § 3).
- [ ] **Decide the terminal-emulator license question** (TECH-STACK.md):
      GPLv3 + Termux widget vs Apache-2.0 + ConnectBot stack. This gates M2
      and possibly a repo split.
- [ ] `MeshBackend` interface + fake in-memory backend so every screen is
      developable without a tailnet.
- [ ] CI: assemble + unit tests on PR.

**Exit criteria**: app builds, shows a fake peer list from the fake backend.

## M1 — MVP on companion mode (medium)

Mesh = Option B (official Tailscale app owns the VPN).

- [ ] Onboarding: detect whether the tunnel is up (probe a tailnet address),
      guide the user through installing/configuring the official app with the
      BigScale server.
- [ ] `PanelClient`: `preauth-key` sign-in flow (stores server/key for the
      guided setup), `os-users` map fetch, `os-user` POST.
- [ ] Peers screen: list from panel API, favorites (Room), detail sheet,
      online notifications (debounced).
- [ ] Settings: connection form, panel sign-in, appearance shell.

**Exit criteria**: a phone with the official Tailscale app connected to a
BigScale server shows the live peer list with correct display names, owners,
and SSH users.

## M2 — Terminal (large — the heart of the app)

- [ ] `core/ssh`: sshj transport per peer, keepalive, reconnect, host-key TOFU
      store, ed25519 keygen + public-key export.
- [ ] Terminal screen with the chosen emulator: PTY resize, UTF-8, 256-color +
      truecolor, extra-keys row, pinch-zoom, selection/copy, URL detection.
- [ ] Tabs + foreground service keeping sessions alive; badge on background
      output.
- [ ] Themes (Dracula/Nord/Catppuccin/Gruvbox/Solarized) + bundled fonts.

**Exit criteria**: run `htop`, `vim`, and `tmux` on a Linux peer for 30
minutes, rotate the phone, background the app for 5 minutes — session intact,
rendering correct.

## M3 — File manager (large)

- [ ] `core/sftp` on the shared SSH transport; listing cache.
- [ ] Browser UI: breadcrumbs, sort, multi-select, rename/delete/permissions.
- [ ] Transfer queue on WorkManager: progress notifications, cancel/retry,
      survive process death.
- [ ] Thumbnails (Coil) for images; text preview; Open-with/Share via
      FileProvider; receive from Android share sheet.

**Exit criteria**: upload 200 photos to a peer with the screen off; download a
1 GB file with the app killed mid-way and see it resume/fail gracefully.

## M4 — Embedded mesh, v1.0 (large, parallelizable with M3)

Mesh = Option A behind the same `MeshBackend`.

- [ ] CI workflow building the pinned `libtailscale` AAR (mirror of the
      desktop `windows.yml` pin-everything philosophy).
- [ ] `IPNService` (VpnService) + foreground notification with
      connect/disconnect actions ("mobile tray icon").
- [ ] Connect flow with desktop parity: control URL + pre-auth key + hostname,
      reset-prefs semantics, post-connect verification (poll self-online, map
      health messages to the same friendly errors as `src/tailscale.rs`).
- [ ] Peer list from LocalAPI (same JSON shapes as desktop), exit-node pick,
      auto-reconnect with backoff, optional always-on VPN.
- [ ] `os_user` POST after connect (profile name as the Android "OS user").

**Exit criteria**: fresh phone, no Tailscale app installed: paste server+key →
Connect → peers appear → terminal + files work. Feature-parity checklist
against desktop README "Highlights" all green (minus SSH/SFTP *launchers*,
which are built-in here).

## M5 — Polish & delight (medium, ongoing)

- [ ] i18n: `.po` → `strings.xml` converter wired into the translation
      pipeline; ship the same 29 locales.
- [ ] `DocumentsProvider`: peers as roots in the system Files app.
- [ ] Snippets library in the terminal; per-host appearance profiles.
- [ ] Latency badge on peer rows (TCP-connect probe — ICMP needs raw sockets).
- [ ] Tablet/foldable layouts (list-detail two-pane); home-screen widget with
      connect toggle + favorite peers.
- [ ] Play Store + F-Droid release pipelines; screenshot automation.

## Explicit non-goals (for now)

- iOS (would force a very different mesh story — NetworkExtension).
- Mosh support (no library-grade Android client worth the maintenance).
- Acting as an SSH/SFTP *server* on the phone.
- Editing files in-app beyond plain-text preview (defer to Open-with).
- Push notifications for peer state while the app is dead (needs server-side
  support; revisit with the panel team).

## Open questions (answer before the matching milestone)

1. **License/repo split** for GPLv3 terminal widget — before M2.
2. Does the panel expose (or can it grow) a peer-list endpoint rich enough for
   companion mode (online state, tags, IPs)? Affects M1 scope — otherwise M1
   falls back to Headscale's API with an admin-scoped key, which is worse.
3. Minimum Android version among the actual BigCommunity user base — 26 is the
   plan; raise to 29+ only with data.
4. Device naming convention for phones (`<user>-phone`?) so panel admins can
   tell them apart.
