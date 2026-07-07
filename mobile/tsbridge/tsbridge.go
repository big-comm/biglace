// Package tsbridge embeds Tailscale's userspace networking stack (tsnet) so the
// Android app can join the tailnet WITHOUT a device-wide VpnService. Only the
// app's own connections (panel HTTP, SSH, SFTP) go over the tunnel, which is all
// this app needs.
//
// It's built into an Android AAR with gomobile:
//
//	gomobile bind -target=android/arm64,android/arm -androidapi 26 \
//	  -o mobile/android/app/libs/tsbridge.aar biglace.community/tsbridge
//
// The Kotlin side calls Start/StatusJSON/ForwardTo/Stop.
package tsbridge

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"strings"
	"sync"
	"time"

	"tailscale.com/client/tailscale"
	"tailscale.com/net/netmon"
	"tailscale.com/tsnet"
)

var (
	mu  sync.Mutex
	srv *tsnet.Server
	lc  *tailscale.LocalClient

	logMu  sync.Mutex
	logBuf []string
)

// appendLog captures a tsnet log line into a bounded ring buffer so the UI can
// show WHY a connection failed (via LastLogs) even without a USB logcat.
func appendLog(format string, args ...any) {
	line := fmt.Sprintf(format, args...)
	logMu.Lock()
	logBuf = append(logBuf, line)
	if len(logBuf) > 300 {
		logBuf = logBuf[len(logBuf)-300:]
	}
	logMu.Unlock()
}

// LastLogs returns the recent engine log lines (newest last).
func LastLogs() string {
	logMu.Lock()
	defer logMu.Unlock()
	return strings.Join(logBuf, "\n")
}

// ifaceJSON is one network interface as seen from the Android/Java side.
type ifaceJSON struct {
	Name      string   `json:"name"`
	Index     int      `json:"index"`
	MTU       int      `json:"mtu"`
	Up        bool     `json:"up"`
	Loopback  bool     `json:"loopback"`
	P2P       bool     `json:"p2p"`
	Multicast bool     `json:"multicast"`
	Addrs     []string `json:"addrs"` // "ip/prefixlen"
}

// SetInterfaces MUST be called before Start on Android. Go's net.Interfaces()
// is blocked on Android 11+ (SDK 30) — it fails with "route ip+net: netlinkrib:
// permission denied", which is fatal to tsnet startup. Tailscale exposes
// netmon.RegisterInterfaceGetter exactly so the app can feed the interface list
// from the Java side (java.net.NetworkInterface, which Android permits). Pass a
// JSON array of interfaces here and tsnet uses it instead of the blocked call.
func SetInterfaces(jsonData string) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("tsbridge SetInterfaces panic: %v", r)
		}
	}()
	var raw []ifaceJSON
	if err := json.Unmarshal([]byte(jsonData), &raw); err != nil {
		return err
	}
	ifs := make([]netmon.Interface, 0, len(raw))
	for _, r := range raw {
		var flags net.Flags
		if r.Up {
			flags |= net.FlagUp
		}
		if r.Loopback {
			flags |= net.FlagLoopback
		}
		if r.P2P {
			flags |= net.FlagPointToPoint
		}
		if r.Multicast {
			flags |= net.FlagMulticast
		}
		ni := &net.Interface{Index: r.Index, MTU: r.MTU, Name: r.Name, Flags: flags}
		var addrs []net.Addr
		for _, a := range r.Addrs {
			ip, ipnet, err := net.ParseCIDR(a)
			if err != nil {
				continue
			}
			ipnet.IP = ip // keep the host address, not the network address
			addrs = append(addrs, ipnet)
		}
		ifs = append(ifs, netmon.Interface{Interface: ni, AltAddrs: addrs})
	}
	netmon.RegisterInterfaceGetter(func() ([]netmon.Interface, error) { return ifs, nil })
	return nil
}

// Start joins the tailnet and blocks until it's up (or fails). Safe to call
// again while running (no-op). stateDir must be a writable, app-private dir.
func Start(controlURL, authKey, hostname, stateDir string) (err error) {
	// A Go panic in tsnet startup would abort the whole process (the app just
	// closes). Recover it into an error so the UI can show what happened.
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("tsbridge Start panic: %v", r)
		}
	}()
	mu.Lock()
	defer mu.Unlock()
	if srv != nil {
		return nil
	}
	// Tailscale's logpolicy panics with "no safe place found to store log
	// state" on Android because os.UserConfigDir() & friends aren't writable.
	// LogsDir() honors TS_LOGS_DIR first, so point it at our writable app dir.
	_ = os.MkdirAll(stateDir, 0o700)
	_ = os.Setenv("TS_LOGS_DIR", stateDir)
	s := &tsnet.Server{
		Dir:        stateDir,
		Hostname:   hostname,
		AuthKey:    authKey,
		ControlURL: controlURL,
		Ephemeral:  false,
		Logf:       appendLog,
		UserLogf:   appendLog,
	}
	if err := s.Start(); err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	if _, err := s.Up(ctx); err != nil {
		_ = s.Close()
		return err
	}
	c, err := s.LocalClient()
	if err != nil {
		_ = s.Close()
		return err
	}
	srv = s
	lc = c
	return nil
}

// Stop tears down the tunnel.
func Stop() error {
	mu.Lock()
	defer mu.Unlock()
	if srv == nil {
		return nil
	}
	err := srv.Close()
	srv = nil
	lc = nil
	return err
}

// Running reports whether the tunnel is up.
func Running() bool {
	mu.Lock()
	defer mu.Unlock()
	return srv != nil
}

// StatusJSON returns the tailnet status (self + peers) as JSON — the same
// ipnstate.Status shape the desktop parses from `tailscale status --json`.
func StatusJSON() (out string, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("tsbridge StatusJSON panic: %v", r)
		}
	}()
	mu.Lock()
	c := lc
	mu.Unlock()
	if c == nil {
		return "", fmt.Errorf("tsbridge: not started")
	}
	st, err := c.Status(context.Background())
	if err != nil {
		return "", err
	}
	b, err := json.Marshal(st)
	if err != nil {
		return "", err
	}
	return string(b), nil
}

// ForwardTo opens a local 127.0.0.1 listener that pipes every accepted
// connection to hostPort (e.g. "100.64.0.5:22") over the tailnet, and returns
// the chosen local port. The Kotlin SSH/SFTP client connects to
// 127.0.0.1:<port>; this is how userspace tsnet reaches tailnet-only addresses
// without a device VPN. The listener lives until Stop() drops the server.
func ForwardTo(hostPort string) (port int, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("tsbridge ForwardTo panic: %v", r)
		}
	}()
	mu.Lock()
	s := srv
	mu.Unlock()
	if s == nil {
		return 0, fmt.Errorf("tsbridge: not started")
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	go func() {
		for {
			local, err := ln.Accept()
			if err != nil {
				return
			}
			go func(local net.Conn) {
				defer local.Close()
				dctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
				remote, err := s.Dial(dctx, "tcp", hostPort)
				cancel()
				if err != nil {
					return
				}
				defer remote.Close()
				go func() { _, _ = io.Copy(remote, local) }()
				_, _ = io.Copy(local, remote)
			}(local)
		}
	}()
	return ln.Addr().(*net.TCPAddr).Port, nil
}
