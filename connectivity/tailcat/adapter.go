// Package tailcatnative exposes Kratos's narrow native Tailcat transport.
//
// It intentionally provides only one fixed Tailcat application port backed by
// one exact IPv4 loopback target. It is suitable for gomobile binding and does
// not install a VPN, execute commands, or expose a general-purpose forwarder.
package tailcatnative

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/netip"
	"os"
	"path/filepath"

	"runtime"
	"sync"
	"time"

	"github.com/tailscale/tailcat"
	"tailscale.com/tailcfg"
	"tailscale.com/types/key"
	"tailscale.com/types/logger"
)

// ApplicationPort is the only remote TCP port exposed by this adapter.
const ApplicationPort uint16 = 7332

const stateVersion = 1

type stateFile struct {
	Version      int                  `json:"version"`
	Role         string               `json:"role"`
	PrivateKey   key.NodePrivate      `json:"private_key"`
	PresharedKey tailcat.PresharedKey `json:"preshared_key,omitempty"`
	Region       *tailcfg.DERPRegion  `json:"region,omitempty"`
}

// Client exposes a loopback HTTP-compatible endpoint connected to a Tailcat
// server's fixed application port.
type Client struct {
	proxy *proxy
	url   string
	once  sync.Once

	mu        sync.Mutex
	tc        *tailcat.Client
	newTC     func() *tailcat.Client
	lastReset time.Time
}

// URL returns the local loopback URL. It remains stable until Close.
func (c *Client) URL() string { return c.url }

// Close stops accepting connections and closes active proxy and Tailcat
// connections. It is safe to call more than once.
func (c *Client) Close() {
	c.once.Do(func() {
		c.proxy.close()
		// proxy.close cancels and joins all dial/copy workers first.
		_ = c.tc.Close()
	})
}

// Server exposes exactly one local IPv4 loopback target through Tailcat's
// application port.
type Server struct {
	proxy   *proxy
	tc      *tailcat.Server
	address string
	once    sync.Once
}

// Address returns the secret Tailcat address clients use to connect.
func (s *Server) Address() string { return s.address }

// Close stops accepting connections and closes active proxy and Tailcat
// connections. It is safe to call more than once.
func (s *Server) Close() {
	s.once.Do(func() {
		s.proxy.close()
		_ = s.tc.Close()
	})
}

// StartClient starts a loopback listener on an OS-selected port and connects
// it to application port 7332 at address. stateDir stores client.key; it is
// deliberately distinct from the server identity. derpMap may be empty.
func StartClient(address, stateDir, derpMap string) (*Client, error) {
	return startClient(address, "127.0.0.1:0", filepath.Join(stateDir, "client.key"), derpMap, true)
}

// StartClientDiagnostic exposes the startup cause for a development build.
// Do not use it in a release UI: the cause can contain transport endpoints.
func StartClientDiagnostic(address, stateDir, derpMap string) (*Client, error) {
	return startClient(address, "127.0.0.1:0", filepath.Join(stateDir, "client.key"), derpMap, false)
}

// StartServer exposes target through application port 7332. target must be an
// exact 127.0.0.1 TCP endpoint. stateDir stores server.key; derpMap may be
// empty. Region selection is automatic.
func StartServer(target, stateDir, derpMap string) (*Server, error) {
	return startServer(target, filepath.Join(stateDir, "server.key"), derpMap, 0, nil)
}

// StartClientCLI is the managed subprocess variant of StartClient. It accepts
// an explicit state file and loopback listener but preserves the same network
// restrictions.
func StartClientCLI(address, listenAddress, statePath, derpMap string) (*Client, error) {
	return startClient(address, listenAddress, statePath, derpMap, true)
}

// StartServerCLI is the managed subprocess variant of StartServer. A non-zero
// regionID pins relay selection to that DERP map region.
func StartServerCLI(target, statePath, derpMap string, regionID int) (*Server, error) {
	return startServer(target, statePath, derpMap, regionID, nil)
}

func startClient(address, listenAddress, statePath, derpMap string, redact bool) (*Client, error) {
	if _, err := tailcat.ParseAddr(tailcat.Addr(address)); err != nil {
		return nil, errors.New("invalid Tailcat address")
	}
	if err := validateLoopback(listenAddress, true); err != nil {
		return nil, fmt.Errorf("invalid listen endpoint: %w", err)
	}
	state, err := loadOrCreateState(statePath, "client")
	if err != nil {
		return nil, fmt.Errorf("client identity: %w", err)
	}
	newTC := func() *tailcat.Client {
		return &tailcat.Client{
			Server:     tailcat.Addr(address),
			Key:        state.PrivateKey,
			DERPMapURL: derpMap,
			Logf:       logger.Discard,
		}
	}
	cl := newTC()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := cl.Ping(ctx); err != nil {
		_ = cl.Close()
		if !redact {
			return nil, fmt.Errorf("Tailcat client startup: %w", err)
		}
		return nil, redactError("Tailcat client startup", err)
	}
	ln, err := net.Listen("tcp4", listenAddress)
	if err != nil {
		_ = cl.Close()
		return nil, errors.New("could not open loopback listener")
	}
	c := &Client{tc: cl, newTC: newTC, url: "http://" + ln.Addr().String()}
	c.proxy = newProxy(ln, c.dial)
	c.proxy.start()
	return c, nil
}

// Bound each tunnel dial: the proxy's lifetime context alone can leave a
// gVisor SYN pending long after the HTTP caller has given up. The pinned
// Tailcat client caches its initial registration (upDone); a restarted server
// has forgotten that peer. Recreate the tunnel, with the SAME persisted key,
// only after a failed dial. Keep the loopback listener and pairing untouched.
const tunnelDialTimeout = 5 * time.Second

func (c *Client) dial(ctx context.Context) (net.Conn, error) {
	c.mu.Lock()
	tc := c.tc
	c.mu.Unlock()
	dial := func(tc *tailcat.Client) (net.Conn, error) {
		attempt, cancel := context.WithTimeout(ctx, tunnelDialTimeout)
		defer cancel()
		return tc.DialTCPPort(attempt, ApplicationPort)
	}
	conn, err := dial(tc)
	if err == nil || ctx.Err() != nil {
		return conn, err
	}
	c.mu.Lock()
	// Coalesce simultaneous room reconnects. Never tear down a newer tunnel
	// because a dial from its predecessor finally timed out; rate-limit resets
	// while the server is offline or its application is unavailable.
	if c.tc == tc && time.Since(c.lastReset) >= tunnelDialTimeout && ctx.Err() == nil {
		_ = tc.Close()
		c.tc = c.newTC()
		c.lastReset = time.Now()
	}
	tc = c.tc
	c.mu.Unlock()
	if ctx.Err() != nil {
		return nil, ctx.Err()
	}
	// No application bytes have been copied yet: retrying the TCP dial cannot
	// replay a command or attachment write. The upper layer owns such retries.
	return dial(tc)
}

func startServer(target, statePath, derpMap string, regionID int, region *tailcfg.DERPRegion) (*Server, error) {
	if err := validateLoopback(target, false); err != nil {
		return nil, fmt.Errorf("invalid target endpoint: %w", err)
	}
	state, err := loadOrCreateState(statePath, "server")
	if err != nil {
		return nil, fmt.Errorf("server identity: %w", err)
	}
	srv := &tailcat.Server{
		Key:          state.PrivateKey,
		PresharedKey: state.PresharedKey,
		DERPMapURL:   derpMap,
		Logf:         logger.Discard,
	}
	switch {
	case region != nil:
		srv.Region = region
	case regionID != 0:
		srv.RegionID = tailcfg.DERPRegionID(regionID)
	case state.Region != nil:
		srv.Region = state.Region
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	ln, err := srv.Listen(ctx, "tcp", fmt.Sprintf(":%d", ApplicationPort))
	if err != nil {
		_ = srv.Close()
		return nil, redactError("Tailcat server startup", err)
	}

	// Persist the selected relay region so the complete secret address remains
	// stable across restarts, not merely the WireGuard identity.
	ci, err := tailcat.ParseAddr(srv.TailcatAddr())
	if err != nil || len(ci.Region) != 1 {
		_ = ln.Close()
		_ = srv.Close()
		return nil, errors.New("Tailcat server produced invalid connection metadata")
	}
	if state.Region == nil || regionID != 0 || region != nil {
		state.Region = ci.Region[0]
		if err := writeState(statePath, state); err != nil {
			_ = ln.Close()
			_ = srv.Close()
			return nil, fmt.Errorf("server identity: %w", err)
		}
	}

	p := newProxy(ln, func(ctx context.Context) (net.Conn, error) {
		var d net.Dialer
		return d.DialContext(ctx, "tcp4", target)
	})
	p.start()
	return &Server{proxy: p, tc: srv, address: string(srv.TailcatAddr())}, nil
}

func validateLoopback(address string, allowZero bool) error {
	ap, err := netip.ParseAddrPort(address)
	if err != nil {
		return errors.New("must be a numeric IP:port")
	}
	if ap.Addr() != netip.MustParseAddr("127.0.0.1") {
		return errors.New("must use 127.0.0.1")
	}
	if ap.Port() == 0 && !allowZero {
		return errors.New("port must not be zero")
	}
	return nil
}

func loadOrCreateState(path, role string) (*stateFile, error) {
	if path == "" {
		return nil, errors.New("state path is empty")
	}
	st, err := os.Lstat(path)
	if err == nil {
		if !st.Mode().IsRegular() || (runtime.GOOS != "windows" && st.Mode().Perm() != 0600) {
			return nil, errors.New("state file must be regular and private")
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, errors.New("could not read state file")
		}
		var state stateFile
		if err := json.Unmarshal(data, &state); err != nil {
			return nil, errors.New("state file is corrupt")
		}
		if err := validateState(&state, role); err != nil {
			return nil, err
		}
		return &state, nil
	}
	if !errors.Is(err, os.ErrNotExist) {
		return nil, errors.New("could not inspect state file")
	}
	state := &stateFile{Version: stateVersion, Role: role, PrivateKey: key.NewNode()}
	if role == "server" {
		state.PresharedKey = tailcat.NewPresharedKey()
	}
	if err := writeState(path, state); err != nil {
		return nil, err
	}
	return state, nil
}

func validateState(state *stateFile, role string) error {
	if state.Version != stateVersion || state.Role != role || state.PrivateKey.IsZero() {
		return errors.New("state file is corrupt or belongs to the other endpoint role")
	}
	if role == "server" && state.PresharedKey.IsZero() {
		return errors.New("server state file is missing its pre-shared key")
	}
	return nil
}

func writeState(path string, state *stateFile) error {
	data, err := json.Marshal(state)
	if err != nil {
		return errors.New("could not encode state file")
	}
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0700); err != nil {
		return errors.New("could not create state directory")
	}
	if err := os.Chmod(dir, 0700); err != nil {
		return errors.New("could not secure state directory")
	}
	tmp, err := os.CreateTemp(dir, ".tailcat-state-*")
	if err != nil {
		return errors.New("could not create state file")
	}
	tmpName := tmp.Name()
	ok := false
	defer func() {
		_ = tmp.Close()
		if !ok {
			_ = os.Remove(tmpName)
		}
	}()
	if err := tmp.Chmod(0600); err != nil {
		return errors.New("could not secure state file")
	}
	if _, err := tmp.Write(data); err != nil {
		return errors.New("could not write state file")
	}
	if err := tmp.Sync(); err != nil {
		return errors.New("could not sync state file")
	}
	if err := tmp.Close(); err != nil {
		return errors.New("could not close state file")
	}
	if err := os.Rename(tmpName, path); err != nil {
		return errors.New("could not install state file")
	}
	ok = true
	if d, err := os.Open(dir); err == nil {
		_ = d.Sync()
		_ = d.Close()
	}
	return nil
}

// redactError deliberately excludes upstream text, which may contain the
// secret Tailcat address or raw relay endpoints.
func redactError(operation string, _ error) error { return errors.New(operation + " failed") }

type proxy struct {
	listener net.Listener
	dial     func(context.Context) (net.Conn, error)
	ctx      context.Context
	cancel   context.CancelFunc
	mu       sync.Mutex
	closed   bool
	active   map[net.Conn]struct{}
	wg       sync.WaitGroup
}

func newProxy(listener net.Listener, dial func(context.Context) (net.Conn, error)) *proxy {
	ctx, cancel := context.WithCancel(context.Background())
	return &proxy{listener: listener, dial: dial, ctx: ctx, cancel: cancel, active: make(map[net.Conn]struct{})}
}

func (p *proxy) start() {
	p.wg.Add(1)
	go func() {
		defer p.wg.Done()
		for {
			local, err := p.listener.Accept()
			if err != nil {
				return
			}
			p.wg.Add(1)
			go p.serve(local)
		}
	}()
}

func (p *proxy) serve(local net.Conn) {
	defer p.wg.Done()
	if !p.track(local) {
		_ = local.Close()
		return
	}
	defer p.untrack(local)
	remote, err := p.dial(p.ctx)
	if err != nil {
		_ = local.Close()
		return
	}
	if !p.track(remote) {
		_ = remote.Close()
		_ = local.Close()
		return
	}
	defer p.untrack(remote)
	proxyBoth(local, remote)
}

func (p *proxy) track(conn net.Conn) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.closed {
		return false
	}
	p.active[conn] = struct{}{}
	return true
}

func (p *proxy) untrack(conn net.Conn) {
	p.mu.Lock()
	delete(p.active, conn)
	p.mu.Unlock()
	_ = conn.Close()
}

func (p *proxy) close() {
	p.cancel()
	_ = p.listener.Close()
	p.mu.Lock()
	p.closed = true
	for conn := range p.active {
		_ = conn.Close()
	}
	p.mu.Unlock()
	p.wg.Wait()
}

func proxyBoth(a, b net.Conn) {
	var wg sync.WaitGroup
	wg.Add(2)
	copyOne := func(dst, src net.Conn) {
		defer wg.Done()
		_, _ = io.Copy(dst, src)
		if cw, ok := dst.(interface{ CloseWrite() error }); ok {
			_ = cw.CloseWrite()
		} else {
			_ = dst.Close()
		}
		if cr, ok := src.(interface{ CloseRead() error }); ok {
			_ = cr.CloseRead()
		}
	}
	go copyOne(a, b)
	go copyOne(b, a)
	wg.Wait()
	_ = a.Close()
	_ = b.Close()
}
