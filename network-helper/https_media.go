package main

// A room-scoped, fixed-SFU route through the signaling server's HTTPS origin.
// Native WebRTC still uses local authenticated TURN; no VPN SDK or OS adapter
// is started. The room JWT goes only in an HTTPS Authorization header.
import (
	"bufio"
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"
)

type httpsMediaProvider struct {
	endpoint  string
	token     string
	own       netip.Addr
	port      uint16
	ctx       context.Context
	cancel    context.CancelFunc
	transport *http.Transport
}

func newHTTPSMediaProvider(endpoint, token string, port uint16) (*httpsMediaProvider, error) {
	return newBoundMediaProvider(endpoint, token, port, false)
}

// The private app has already authenticated and pinned this iroh gateway. Only
// its numeric loopback socket is reachable here; plaintext remote WS stays denied.
func newIrohHTTPSMediaProvider(endpoint, gateway, token string, port uint16) (*httpsMediaProvider, error) {
	u, err := url.Parse(endpoint)
	if err != nil || u.Scheme != "ws" || u.Port() != "4580" || u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.Opaque != "" || u.Path != "/api/rtc/media" || u.RawPath != "" {
		return nil, errors.New("invalid private media origin")
	}
	id := strings.TrimSuffix(u.Hostname(), ".iroh")
	if id == u.Hostname() || len(id) != 64 || strings.Trim(id, "0123456789abcdef") != "" {
		return nil, errors.New("invalid private media endpoint")
	}
	g, err := url.Parse(gateway)
	if err != nil || len(gateway) > 200 || g.Scheme != "http" || g.Hostname() != "127.0.0.1" || g.Port() == "" || g.Port() == "0" || g.User != nil || g.RawQuery != "" || g.Fragment != "" || g.Opaque != "" || g.RawPath != "" || (g.Path != "" && g.Path != "/") {
		return nil, errors.New("owned numeric loopback gateway required")
	}
	g.Scheme = "ws"
	g.Path = "/api/rtc/media"
	return newBoundMediaProvider(g.String(), token, port, true)
}
func newBoundMediaProvider(endpoint, token string, port uint16, local bool) (*httpsMediaProvider, error) {
	u, err := url.Parse(endpoint)
	if err != nil || !(u.Scheme == "wss" && !local || local && u.Scheme == "ws" && u.Hostname() == "127.0.0.1" && u.Port() != "" && u.Port() != "0") || u.Hostname() == "" || u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.Opaque != "" || len(endpoint) > 2048 || u.Path != "/api/rtc/media" || u.RawPath != "" || port == 0 || len(token) == 0 || len(token) > 16384 {
		return nil, errors.New("fixed HTTPS media endpoint and room ticket required")
	}
	if strings.Count(token, ".") != 2 {
		return nil, errors.New("invalid room ticket")
	}
	for _, b := range []byte(token) {
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '_' || b == '-' || b == '.') {
			return nil, errors.New("invalid room ticket")
		}
	}
	// Reuse the existing TURN allocator's private virtual address convention.
	base, err := newIrohMediaProvider("", "100.127.255.254", port)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	transport.ForceAttemptHTTP2 = false
	transport.TLSNextProto = map[string]func(string, *tls.Conn) http.RoundTripper{}
	if transport.TLSClientConfig == nil {
		transport.TLSClientConfig = &tls.Config{}
	} else {
		transport.TLSClientConfig = transport.TLSClientConfig.Clone()
	}
	transport.TLSClientConfig.NextProtos = []string{"http/1.1"}
	transport.TLSClientConfig.InsecureSkipVerify = false
	transport.DialContext = (&net.Dialer{Timeout: 5 * time.Second, KeepAlive: 30 * time.Second}).DialContext
	transport.TLSHandshakeTimeout = 5 * time.Second
	transport.ResponseHeaderTimeout = 5 * time.Second
	transport.DisableKeepAlives = true
	return &httpsMediaProvider{endpoint: u.String(), token: token, port: port, own: base.own, ctx: ctx, cancel: cancel, transport: transport}, nil
}
func (p *httpsMediaProvider) TailscaleIPs() (netip.Addr, netip.Addr) { return p.own, netip.Addr{} }
func (p *httpsMediaProvider) ListenPacket(network, address string) (net.PacketConn, error) {
	if network != "udp4" || address != net.JoinHostPort(p.own.String(), "0") {
		return nil, errors.New("only an ephemeral media allocation is permitted")
	}
	transport := p.transport.Clone()
	var local net.Addr
	dial := transport.DialContext
	transport.DialContext = func(ctx context.Context, network, address string) (net.Conn, error) {
		conn, err := dial(ctx, network, address)
		if err == nil {
			local = conn.LocalAddr()
		}
		return conn, err
	}
	defer transport.CloseIdleConnections()
	client := &http.Client{Transport: transport, CheckRedirect: func(*http.Request, []*http.Request) error { return errors.New("media redirects are not permitted") }}
	ctx, cancel := context.WithTimeout(p.ctx, 5*time.Second)
	defer cancel()
	headers := make(http.Header)
	headers.Set("Authorization", "Bearer "+p.token)
	conn, _, err := websocket.Dial(ctx, p.endpoint, &websocket.DialOptions{HTTPClient: client, HTTPHeader: headers})
	if err != nil {
		return nil, errors.New("secure media connection was refused")
	}
	source, ok := local.(*net.TCPAddr)
	if !ok || source.Port == 0 {
		conn.CloseNow()
		return nil, errors.New("media socket ownership could not be confirmed")
	}
	stream := websocket.NetConn(p.ctx, conn, websocket.MessageBinary)
	conn.SetReadLimit(maxMediaDatagram + 2)
	return &iroMediaPacket{peer: &net.UDPAddr{IP: net.IP(netip.MustParseAddr("100.127.255.254").AsSlice()), Port: int(p.port)},
		address: &net.UDPAddr{IP: net.IP(p.own.AsSlice()), Port: source.Port}, stream: stream, reader: bufio.NewReaderSize(stream, 4096)}, nil
}

type httpsMediaLease struct {
	turn     *LocalTurn
	provider *httpsMediaProvider
	once     sync.Once
}

func (l *httpsMediaLease) Close() {
	l.once.Do(func() { l.provider.cancel(); _ = l.turn.Close(); l.provider.transport.CloseIdleConnections() })
}

func (h *helper) openHTTPSMedia(w http.ResponseWriter, r *http.Request) {
	h.openBoundMedia(w, r, false)
}
func (h *helper) openIrohHTTPSMedia(w http.ResponseWriter, r *http.Request) {
	h.openBoundMedia(w, r, true)
}
func (h *helper) openBoundMedia(w http.ResponseWriter, r *http.Request, private bool) {
	var input struct {
		URL     string `json:"url"`
		Token   string `json:"token"`
		Port    uint16 `json:"peer_port"`
		Gateway string `json:"gateway,omitempty"`
	}
	r.Body = http.MaxBytesReader(w, r.Body, 20*1024)
	decoder := json.NewDecoder(r.Body)
	decoder.DisallowUnknownFields()
	if decoder.Decode(&input) != nil || decoder.Decode(new(any)) != io.EOF {
		controlError(w, 400, "Invalid secure media request")
		return
	}
	var provider *httpsMediaProvider
	var err error
	if private {
		provider, err = newIrohHTTPSMediaProvider(input.URL, input.Gateway, input.Token, input.Port)
	} else if input.Gateway != "" {
		err = errors.New("unexpected private gateway")
	} else {
		provider, err = newHTTPSMediaProvider(input.URL, input.Token, input.Port)
	}
	if err != nil {
		controlError(w, 400, "Invalid secure media route")
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed {
		provider.cancel()
		controlError(w, 409, "Connection helper stopped")
		return
	}
	if len(h.httpsMedia) >= 8 {
		provider.cancel()
		controlError(w, 429, "Media route limit reached")
		return
	}
	turn, err := newLocalTurn(provider, func(ip net.IP) bool { return ip.Equal(net.IP(netip.MustParseAddr("100.127.255.254").AsSlice())) }, func(addr *net.UDPAddr) bool {
		return addr.Port == int(provider.port) && addr.IP.Equal(net.IP(netip.MustParseAddr("100.127.255.254").AsSlice()))
	})
	if err != nil {
		provider.cancel()
		controlError(w, 502, "Cannot start secure media")
		return
	}
	id, err := randomMediaCredential()
	if err != nil {
		provider.cancel()
		_ = turn.Close()
		controlError(w, 502, "Cannot create media lease")
		return
	}
	if h.httpsMedia == nil {
		h.httpsMedia = make(map[string]*httpsMediaLease)
	}
	h.httpsMedia[id] = &httpsMediaLease{turn: turn, provider: provider}
	turn.Config.Address = provider.own.String()
	jsonResponse(w, 200, struct {
		ID   string     `json:"id"`
		TURN TURNConfig `json:"turn"`
	}{id, turn.Config})
}
func (h *helper) closeHTTPSMedia(w http.ResponseWriter, r *http.Request) {
	var input struct {
		ID string `json:"id"`
	}
	if decodeControl(w, r, &input) != nil || len(input.ID) != 43 {
		controlError(w, 400, "Invalid media lease")
		return
	}
	h.mu.Lock()
	lease := h.httpsMedia[input.ID]
	delete(h.httpsMedia, input.ID)
	h.mu.Unlock()
	if lease != nil {
		lease.Close()
	}
	jsonResponse(w, 200, map[string]bool{"closed": true})
}
