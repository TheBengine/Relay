package main

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/http/httputil"
	"net/netip"
	"net/url"
	"strings"
	"sync"
	"time"
)

const relayPort = 4580

type relayNetwork interface {
	NetPacketProvider
	Dial(context.Context, string, string) (net.Conn, error)
	Listen(string, string) (net.Listener, error)
	Connect(context.Context) error
	Close() error
}

func strictTailIP(value string) (netip.Addr, error) {
	ip, err := netip.ParseAddr(value)
	if err != nil || !ip.Is4() || ip.String() != value || !netip.MustParsePrefix("100.64.0.0/10").Contains(ip) {
		return netip.Addr{}, errors.New("use an exact private network IPv4 address")
	}
	return ip, nil
}

// Both gateways are fixed-target services, never HTTP CONNECT/open proxies.
// Path normalization is deliberately conservative before forwarding: the Rust
// peer listener independently denies private/local management capabilities.
func safeRelayRequest(r *http.Request) bool {
	if _, present := r.Header["Origin"]; present {
		return false
	}
	if r.Header.Get("Sec-Fetch-Site") != "" || r.Method == http.MethodConnect {
		return false
	}
	p := r.URL.Path
	if strings.ContainsAny(p, "\\\x00\r\n") || strings.Contains(p, "%") {
		return false
	}
	for _, part := range strings.Split(p, "/") {
		if part == "." || part == ".." {
			return false
		}
	}
	lower := strings.ToLower(p)
	parts := strings.FieldsFunc(lower, func(c rune) bool { return c == '/' })
	if len(parts) > 0 && (parts[0] == "host-secret" || parts[0] == "local-management" || parts[0] == "localapi") {
		return false
	}
	if len(parts) > 1 && parts[0] == "api" && (parts[1] == "local" || parts[1] == "host-secret" || parts[1] == "local-management") {
		return false
	}
	return p == "/health" || p == "/api/health" || strings.HasPrefix(p, "/api/") || p == "/rtc" || strings.HasPrefix(p, "/rtc/")
}

func stripIngressHeaders(h http.Header) {
	for key := range h {
		lower := strings.ToLower(key)
		if lower == "forwarded" || strings.HasPrefix(lower, "x-forwarded-") || strings.HasPrefix(lower, "x-relay-network-") || lower == "x-relay-local-control" || lower == "x-relay-instance-id" || lower == "x-relay-server-id" || lower == "proxy-authorization" {
			delete(h, key)
		}
	}
}

func fixedProxy(target *url.URL, transport http.RoundTripper, ingressToken string) http.Handler {
	proxy := &httputil.ReverseProxy{
		Transport: transport, FlushInterval: -1,
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.SetURL(target)
			pr.Out.Host = target.Host
			stripIngressHeaders(pr.Out.Header)
			if ingressToken != "" {
				host, _, err := net.SplitHostPort(pr.In.RemoteAddr)
				ip, parseErr := strictTailIP(host)
				if err == nil && parseErr == nil {
					pr.Out.Header.Set("X-Relay-Network-Ingress", ingressToken)
					pr.Out.Header.Set("X-Relay-Network-Peer", ip.String())
				}
			}
		},
		ErrorHandler: func(w http.ResponseWriter, _ *http.Request, _ error) {
			http.Error(w, "Private network connection unavailable", http.StatusBadGateway)
		},
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !safeRelayRequest(r) {
			http.Error(w, "Not available", http.StatusForbidden)
			return
		}
		if ingressToken != "" {
			host, _, err := net.SplitHostPort(r.RemoteAddr)
			if _, invalid := strictTailIP(host); err != nil || invalid != nil {
				http.Error(w, "Private network peer required", http.StatusForbidden)
				return
			}
		}
		proxy.ServeHTTP(w, r)
	})
}

type ownedGateway struct {
	listener  net.Listener
	server    *http.Server
	transport *http.Transport
	url       string
}

// Server.Close does not close hijacked WebSockets; the listener owns those
// connections too, so helper shutdown releases chat, voice and RTC streams.
type ownedListener struct {
	net.Listener
	mu          sync.Mutex
	closed      bool
	connections map[*ownedConn]struct{}
}
type ownedConn struct {
	net.Conn
	owner *ownedListener
	once  sync.Once
}

func (c *ownedConn) Close() error {
	var err error
	c.once.Do(func() { err = c.Conn.Close(); c.owner.mu.Lock(); delete(c.owner.connections, c); c.owner.mu.Unlock() })
	return err
}
func trackListener(listener net.Listener) *ownedListener {
	return &ownedListener{Listener: listener, connections: make(map[*ownedConn]struct{})}
}
func (l *ownedListener) Accept() (net.Conn, error) {
	conn, err := l.Listener.Accept()
	if err != nil {
		return nil, err
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.closed {
		_ = conn.Close()
		return nil, net.ErrClosed
	}
	owned := &ownedConn{Conn: conn, owner: l}
	l.connections[owned] = struct{}{}
	return owned, nil
}
func (l *ownedListener) Close() error {
	l.mu.Lock()
	l.closed = true
	connections := make([]*ownedConn, 0, len(l.connections))
	for conn := range l.connections {
		connections = append(connections, conn)
	}
	l.mu.Unlock()
	err := l.Listener.Close()
	for _, conn := range connections {
		_ = conn.Close()
	}
	return err
}

func (g *ownedGateway) Close() error {
	g.transport.CloseIdleConnections()
	err := g.server.Close()
	_ = g.listener.Close()
	return err
}

func newClientGateway(network relayNetwork, host netip.Addr) (*ownedGateway, error) {
	physical, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	listener := trackListener(physical)
	target := &url.URL{Scheme: "http", Host: net.JoinHostPort(host.String(), "4580")}
	transport := &http.Transport{Proxy: nil, DialContext: func(ctx context.Context, _ string, address string) (net.Conn, error) {
		if address != target.Host {
			return nil, errors.New("unexpected private network target")
		}
		return network.Dial(ctx, "tcp", target.Host)
	}, TLSHandshakeTimeout: 5 * time.Second, ResponseHeaderTimeout: 15 * time.Second, MaxIdleConns: 8, MaxIdleConnsPerHost: 8, IdleConnTimeout: 30 * time.Second}
	gateway := &ownedGateway{listener: listener, transport: transport, url: "http://" + listener.Addr().String()}
	gateway.server = &http.Server{Handler: fixedProxy(target, transport, ""), ReadHeaderTimeout: 5 * time.Second, MaxHeaderBytes: 32 * 1024}
	go func() { _ = gateway.server.Serve(listener) }()
	return gateway, nil
}

func newHostGateway(network relayNetwork, port uint16, token string) (*ownedGateway, error) {
	if port == 0 || port == relayPort || len(token) < 32 || len(token) > 256 {
		return nil, errors.New("a separate private peer listener and ingress capability are required")
	}
	physical, err := network.Listen("tcp", ":4580")
	if err != nil {
		return nil, err
	}
	listener := trackListener(physical)
	target := &url.URL{Scheme: "http", Host: net.JoinHostPort("127.0.0.1", itoa(port))}
	transport := &http.Transport{Proxy: nil, DialContext: (&net.Dialer{Timeout: 5 * time.Second}).DialContext, ResponseHeaderTimeout: 15 * time.Second, MaxIdleConns: 16, MaxIdleConnsPerHost: 16, IdleConnTimeout: 30 * time.Second}
	gateway := &ownedGateway{listener: listener, transport: transport}
	gateway.server = &http.Server{Handler: fixedProxy(target, transport, token), ReadHeaderTimeout: 5 * time.Second, MaxHeaderBytes: 32 * 1024}
	go func() { _ = gateway.server.Serve(listener) }()
	return gateway, nil
}
