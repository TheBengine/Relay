package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"strings"
	"sync"
	"testing"
	"time"
)

type fakeRelayNetwork struct {
	*testPacketProvider
	mu       sync.Mutex
	connects int
	closed   bool
	backend  string
	dialed   []string
	conns    []net.Conn
	udpSinks []*net.UDPConn
	gate     chan struct{}
	auth     func(string)
}

func newFakeRelayNetwork() *fakeRelayNetwork {
	return &fakeRelayNetwork{testPacketProvider: newTestPacketMesh().provider("100.100.5.6")}
}
func (n *fakeRelayNetwork) Connect(ctx context.Context) error {
	n.mu.Lock()
	n.connects++
	n.mu.Unlock()
	if n.auth != nil {
		n.auth("https://login.tailscale.com/a/fixture-only-auth")
	}
	if n.gate != nil {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-n.gate:
		}
	}
	return nil
}
func (n *fakeRelayNetwork) Dial(ctx context.Context, network, address string) (net.Conn, error) {
	n.mu.Lock()
	n.dialed = append(n.dialed, address)
	if network == "udp" && address == "100.100.5.7:4581" {
		sink, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
		if err != nil {
			n.mu.Unlock()
			return nil, err
		}
		conn, err := net.DialUDP("udp4", nil, sink.LocalAddr().(*net.UDPAddr))
		if err != nil {
			_ = sink.Close()
			n.mu.Unlock()
			return nil, err
		}
		n.udpSinks = append(n.udpSinks, sink)
		n.conns = append(n.conns, conn)
		n.mu.Unlock()
		return conn, nil
	}
	backend := n.backend
	n.mu.Unlock()
	if address != "100.100.5.7:4580" || network != "tcp" || backend == "" {
		return nil, errors.New("unconfigured fake route")
	}
	conn, err := (&net.Dialer{}).DialContext(ctx, "tcp4", backend)
	if err == nil {
		n.mu.Lock()
		n.conns = append(n.conns, conn)
		n.mu.Unlock()
	}
	return conn, err
}
func (n *fakeRelayNetwork) Listen(network, address string) (net.Listener, error) {
	if network != "tcp" || address != ":4580" {
		return nil, errors.New("unexpected host listen")
	}
	return net.Listen("tcp4", "127.0.0.1:0")
}
func (n *fakeRelayNetwork) Close() error {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.closed = true
	for _, conn := range n.conns {
		_ = conn.Close()
	}
	for _, sink := range n.udpSinks {
		_ = sink.Close()
	}
	return nil
}

func TestPrivateProxyDeniesLocalPathsAndPreservesVoiceAdmission(t *testing.T) {
	var mu sync.Mutex
	seen := []http.Header{}
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		seen = append(seen, r.Header.Clone())
		mu.Unlock()
		_ = json.NewEncoder(w).Encode(r.URL.RequestURI())
	}))
	defer backend.Close()
	target, _ := url.Parse(backend.URL)
	proxy := fixedProxy(target, http.DefaultTransport, strings.Repeat("c", 64))
	forbidden := []string{"/api/local", "/api/local/shutdown", "/api/local/discord-credentials", "/api/local/extra?x=1", "//api//local/shutdown", "/API/LOCAL/shutdown", "/api/%6cocal/shutdown", "/api%2flocal%2fshutdown", "/api%252flocal%252fshutdown", "/api/../api/local/shutdown", "/api%5clocal/shutdown", "/localapi/v0/status", "/host-secret", "/local-management", "/anything"}
	for _, path := range forbidden {
		for _, method := range []string{"GET", "POST", "PUT", "DELETE", "PATCH"} {
			r := httptest.NewRequest(method, path, nil)
			r.RemoteAddr = "100.100.5.7:54321"
			r.Header.Set("X-Relay-Local-Control", "even-a-real-control-proof-must-not-pass")
			r.Header.Set("Authorization", "Bearer management-or-session-token")
			w := httptest.NewRecorder()
			proxy.ServeHTTP(w, r)
			if w.Code != 403 {
				t.Fatalf("%s %s: %d", method, path, w.Code)
			}
		}
	}
	mu.Lock()
	if len(seen) != 0 {
		t.Fatal("private request reached backend")
	}
	mu.Unlock()
	r := httptest.NewRequest("GET", "/api/voice/tunnel?channel=fixture", nil)
	r.RemoteAddr = "100.100.5.7:54321"
	for name, value := range map[string]string{"X-Relay-Voice-Session": "fixture-voice-session", "X-Relay-Voice-Key": "fixture-voice-key", "Authorization": "Bearer fixture-account", "Forwarded": "for=127.0.0.1", "X-Forwarded-For": "127.0.0.1", "X-Relay-Local-Control": "leaked-control", "X-Relay-Instance-Id": "fake-instance", "X-Relay-Server-Id": "fake-server", "X-Relay-Network-Ingress": "caller-spoof", "X-Relay-Network-Peer": "caller-spoof"} {
		r.Header.Set(name, value)
	}
	w := httptest.NewRecorder()
	proxy.ServeHTTP(w, r)
	if w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	mu.Lock()
	got := seen[0]
	mu.Unlock()
	if got.Get("X-Relay-Voice-Session") != "fixture-voice-session" || got.Get("X-Relay-Voice-Key") != "fixture-voice-key" || got.Get("Authorization") != "Bearer fixture-account" {
		t.Fatal("ordinary account/voice admission was lost")
	}
	for _, name := range []string{"Forwarded", "X-Forwarded-For", "X-Relay-Local-Control", "X-Relay-Instance-Id", "X-Relay-Server-Id"} {
		if got.Get(name) != "" {
			t.Fatalf("private/spoof header forwarded: %s", name)
		}
	}
	if got.Get("X-Relay-Network-Ingress") != strings.Repeat("c", 64) || got.Get("X-Relay-Network-Peer") != "100.100.5.7" {
		t.Fatal("true network ingress proof/source not set")
	}
}

func TestClientGatewayHasOneFixedTargetAndClosesConnections(t *testing.T) {
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { _, _ = io.WriteString(w, r.URL.RequestURI()) }))
	defer backend.Close()
	n := newFakeRelayNetwork()
	n.backend = strings.TrimPrefix(backend.URL, "http://")
	gateway, err := newClientGateway(n, netip.MustParseAddr("100.100.5.7"))
	if err != nil {
		t.Fatal(err)
	}
	defer gateway.Close()
	defer n.Close()
	response, err := http.Get(gateway.url + "/api/messages?before=1")
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(response.Body)
	_ = response.Body.Close()
	if string(body) != "/api/messages?before=1" {
		t.Fatal(string(body))
	}
	// An attacker-supplied authority never selects the network destination.
	request, _ := http.NewRequest("GET", gateway.url+"/api/messages", nil)
	request.Host = "127.0.0.1:2019"
	response, err = http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	browser, _ := http.NewRequest("GET", gateway.url+"/api/messages", nil)
	browser.Header["Origin"] = []string{""}
	response, err = http.DefaultClient.Do(browser)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	if response.StatusCode != 403 {
		t.Fatal("empty browser Origin was accepted")
	}
	n.mu.Lock()
	for _, address := range n.dialed {
		if address != "100.100.5.7:4580" {
			t.Fatal("open proxy target", address)
		}
	}
	n.mu.Unlock()
	if err = gateway.Close(); err != nil && !errors.Is(err, net.ErrClosed) {
		t.Fatal(err)
	}
	if _, err = net.DialTimeout("tcp4", strings.TrimPrefix(gateway.url, "http://"), time.Second); err == nil {
		t.Fatal("route listener survived cleanup")
	}
}

func TestTrackedListenerClosesHijackedConnections(t *testing.T) {
	physical, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	listener := trackListener(physical)
	server := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, buffer, err := w.(http.Hijacker).Hijack()
		if err != nil {
			return
		}
		_, _ = buffer.WriteString("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: fixture\r\n\r\n")
		_ = buffer.Flush()
		_ = conn.SetReadDeadline(time.Now().Add(2 * time.Second))
		var b [1]byte
		_, _ = conn.Read(b[:])
		_ = conn.Close()
	})}
	go server.Serve(listener)
	client, err := net.Dial("tcp4", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	_, _ = io.WriteString(client, "GET /api/events HTTP/1.1\r\nHost: fixture\r\nConnection: Upgrade\r\nUpgrade: fixture\r\n\r\n")
	_ = client.SetReadDeadline(time.Now().Add(time.Second))
	var b [512]byte
	if _, err = client.Read(b[:]); err != nil {
		t.Fatal(err)
	}
	_ = server.Close()
	_ = listener.Close()
	_ = client.SetReadDeadline(time.Now().Add(time.Second))
	if _, err = client.Read(b[:]); err == nil {
		t.Fatal("hijacked connection survived owned-listener close")
	}
}

func TestHostGatewayCannotTargetOrdinaryOwnerPort(t *testing.T) {
	n := newFakeRelayNetwork()
	if _, err := newHostGateway(n, 4580, strings.Repeat("a", 64)); err == nil {
		t.Fatal("ordinary owner listener allowed")
	}
	if _, err := newHostGateway(n, 0, strings.Repeat("a", 64)); err == nil {
		t.Fatal("unspecified peer port allowed")
	}
}
