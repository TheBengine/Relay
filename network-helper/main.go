package main

import (
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"golang.org/x/sys/windows"
	"tailscale.com/envknob"
	"tailscale.com/tsnet"
)

func itoa(value uint16) string { return strconv.Itoa(int(value)) }

type sdkNetwork struct {
	server   *tsnet.Server
	stateDir string
	startMu  sync.Mutex
	started  bool
	closed   bool
}

func (n *sdkNetwork) Connect(ctx context.Context) error {
	if err := protectStateDirectory(n.stateDir); err != nil {
		return err
	}
	n.startMu.Lock()
	if n.closed {
		n.startMu.Unlock()
		return errors.New("private network closed")
	}
	err := n.server.Start()
	n.started = true
	n.startMu.Unlock()
	if err != nil {
		return err
	}
	_, err = n.server.Up(ctx)
	return err
}
func (n *sdkNetwork) Dial(ctx context.Context, network, address string) (net.Conn, error) {
	return n.server.Dial(ctx, network, address)
}
func (n *sdkNetwork) Listen(network, address string) (net.Listener, error) {
	return n.server.Listen(network, address)
}
func (n *sdkNetwork) ListenPacket(network, address string) (net.PacketConn, error) {
	return n.server.ListenPacket(network, address)
}
func (n *sdkNetwork) TailscaleIPs() (netip.Addr, netip.Addr) { return n.server.TailscaleIPs() }
func (n *sdkNetwork) Close() error {
	n.startMu.Lock()
	defer n.startMu.Unlock()
	n.closed = true
	if n.started {
		return n.server.Close()
	}
	return nil
}

type helperStatus struct {
	Phase   string `json:"phase"`
	Message string `json:"message"`
	AuthURL string `json:"auth_url,omitempty"`
	OwnIP   string `json:"address,omitempty"`
}
type helper struct {
	mu         sync.Mutex
	token      string
	status     helperStatus
	network    relayNetwork
	factory    func(func(string)) relayNetwork
	media      *MediaRelay
	irohMedia  map[string]*LocalTurn
	httpsMedia map[string]*httpsMediaLease
	routes     map[string]*ownedGateway
	voices     map[string]*ClientVoiceRelay
	host       *ownedGateway
	hostVoice  *HostUDPRelay
	serverID   string
	cancel     context.CancelFunc
	closed     bool
	stop       chan struct{}
}

func newHelper(token string, factory func(func(string)) relayNetwork) *helper {
	return &helper{token: token, status: helperStatus{Phase: "idle", Message: "Connect the private network when you are ready."}, factory: factory, routes: make(map[string]*ownedGateway), voices: make(map[string]*ClientVoiceRelay), stop: make(chan struct{})}
}

func (h *helper) captureAuthURL(value string) {
	if len(value) > 4096 {
		return
	}
	u, err := url.Parse(value)
	if err != nil || u.Scheme != "https" || u.User != nil || u.Port() != "" || (u.Host != "login.tailscale.com" && u.Host != "controlplane.tailscale.com") || !strings.HasPrefix(u.Path, "/a/") {
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if !h.closed && (h.status.Phase == "connecting" || h.status.Phase == "needs_login") {
		h.status.AuthURL = value
		h.status.Phase = "needs_login"
		h.status.Message = "Sign in to connect your private Relay network."
	}
}

func (h *helper) connect() {
	h.mu.Lock()
	if h.closed || h.status.Phase == "connecting" || h.status.Phase == "needs_login" || h.status.Phase == "ready" {
		h.mu.Unlock()
		return
	}
	// A failed node must be closed before a deliberate retry uses its state.
	if h.network != nil {
		h.mu.Unlock()
		return
	}
	h.status = helperStatus{Phase: "connecting", Message: "Connecting your private Relay network."}
	n := h.factory(h.captureAuthURL)
	h.network = n
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	h.cancel = cancel
	h.mu.Unlock()
	go func() {
		err := n.Connect(ctx)
		h.mu.Lock()
		defer h.mu.Unlock()
		if h.closed {
			return
		}
		if err != nil {
			h.status = helperStatus{Phase: "error", Message: "Private network connection failed. Reopen Relay and try again."}
			return
		}
		own, _ := n.TailscaleIPs()
		if _, err := strictTailIP(own.String()); err != nil {
			h.status = helperStatus{Phase: "error", Message: "Private network address unavailable."}
			return
		}
		media, err := NewMediaRelay(n)
		if err != nil {
			h.status = helperStatus{Phase: "error", Message: "Private media transport unavailable."}
			return
		}
		h.media = media
		h.status = helperStatus{Phase: "ready", OwnIP: own.String(), Message: "Your private Relay network is connected."}
	}()
}

func (h *helper) close() {
	h.mu.Lock()
	if h.closed {
		h.mu.Unlock()
		return
	}
	h.closed = true
	h.status = helperStatus{Phase: "stopped", Message: "Private network stopped."}
	if h.cancel != nil {
		h.cancel()
	}
	routes := h.routes
	voices := h.voices
	host := h.host
	hostVoice := h.hostVoice
	media := h.media
	irohMedia := h.irohMedia
	httpsMedia := h.httpsMedia
	n := h.network
	h.routes = make(map[string]*ownedGateway)
	h.irohMedia = nil
	h.httpsMedia = nil
	h.voices = make(map[string]*ClientVoiceRelay)
	h.host = nil
	close(h.stop)
	h.mu.Unlock()
	for _, voice := range voices {
		_ = voice.Close()
	}
	for _, lease := range httpsMedia {
		lease.Close()
	}
	for _, turn := range irohMedia {
		_ = turn.Close()
	}
	if hostVoice != nil {
		_ = hostVoice.Close()
	}
	for _, gateway := range routes {
		_ = gateway.Close()
	}
	if host != nil {
		_ = host.Close()
	}
	if media != nil {
		_ = media.Close()
	}
	// Close only an SDK which Connect has started. tsnet.Close cannot be called
	// concurrently with Start; connection worker is cancelled before cleanup.
	if n != nil {
		_ = n.Close()
	}
}

func (h *helper) authorized(r *http.Request) bool {
	if _, present := r.Header["Origin"]; present {
		return false
	}
	if r.Header.Get("Sec-Fetch-Site") != "" {
		return false
	}
	supplied := r.Header.Get("Authorization")
	if !strings.HasPrefix(supplied, "Bearer ") {
		return false
	}
	got := sha256.Sum256([]byte(strings.TrimPrefix(supplied, "Bearer ")))
	want := sha256.Sum256([]byte(h.token))
	return subtle.ConstantTimeCompare(got[:], want[:]) == 1
}

func decodeControl(w http.ResponseWriter, r *http.Request, value any) error {
	r.Body = http.MaxBytesReader(w, r.Body, 4096)
	decoder := json.NewDecoder(r.Body)
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(value); err != nil {
		return err
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return errors.New("one JSON object is required")
	}
	return nil
}
func jsonResponse(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}
func controlError(w http.ResponseWriter, status int, message string) {
	jsonResponse(w, status, map[string]string{"error": message})
}

func (h *helper) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if !h.authorized(r) {
		controlError(w, http.StatusForbidden, "Private app control required")
		return
	}
	switch {
	case r.URL.Path == "/https-media" && r.Method == http.MethodPost:
		h.openHTTPSMedia(w, r)
	case r.URL.Path == "/iroh-https-media" && r.Method == http.MethodPost:
		h.openIrohHTTPSMedia(w, r)
	case r.URL.Path == "/close-https-media" && r.Method == http.MethodPost:
		h.closeHTTPSMedia(w, r)
	case r.URL.Path == "/iroh-media" && r.Method == http.MethodPost:
		h.openIrohMedia(w, r)
	case r.URL.Path == "/status" && r.Method == http.MethodGet:
		h.mu.Lock()
		status := h.status
		h.mu.Unlock()
		jsonResponse(w, 200, status)
	case r.URL.Path == "/connect" && r.Method == http.MethodPost:
		h.connect()
		h.mu.Lock()
		status := h.status
		h.mu.Unlock()
		jsonResponse(w, 200, status)
	case r.URL.Path == "/route" && r.Method == http.MethodPost:
		var input struct {
			Host         string  `json:"host"`
			Port         uint16  `json:"port"`
			MediaUDPPort *uint16 `json:"media_udp_port,omitempty"`
			VoiceUDPPort *uint16 `json:"voice_udp_port,omitempty"`
		}
		if decodeControl(w, r, &input) != nil {
			controlError(w, 400, "Invalid route request")
			return
		}
		host, err := strictTailIP(input.Host)
		mediaPort := uint16(7882)
		if input.MediaUDPPort != nil {
			mediaPort = *input.MediaUDPPort
		}
		voicePort := uint16(4581)
		if input.VoiceUDPPort != nil {
			voicePort = *input.VoiceUDPPort
		}
		if err != nil || input.Port != relayPort || mediaPort == 0 || voicePort == 0 {
			controlError(w, 400, "A private Relay address on port4580 is required")
			return
		}
		h.mu.Lock()
		defer h.mu.Unlock()
		if h.closed || h.status.Phase != "ready" {
			controlError(w, 409, "Connect the private network first")
			return
		}
		if len(h.routes) >= 16 && h.routes[input.Host] == nil {
			controlError(w, 429, "Too many private Relay connections")
			return
		}
		turn, err := h.media.AddPeer(host, mediaPort)
		if err != nil {
			controlError(w, 502, "Private media route unavailable")
			return
		}
		gateway := h.routes[input.Host]
		if gateway == nil {
			gateway, err = newClientGateway(h.network, host)
			if err != nil {
				controlError(w, 502, "Private Relay route unavailable")
				return
			}
			h.routes[input.Host] = gateway
		}
		voice, err := NewClientVoiceRelay(h.network, host, voicePort)
		if err != nil {
			controlError(w, 502, "Private voice route unavailable")
			return
		}
		if old := h.voices[input.Host]; old != nil {
			_ = old.Close()
		}
		h.voices[input.Host] = voice
		jsonResponse(w, 200, map[string]any{"http_url": gateway.url, "turn": turn, "voice_udp_port": voice.Port()})
	case r.URL.Path == "/path" && r.Method == http.MethodPost:
		var input struct {
			Host string `json:"host"`
		}
		if decodeControl(w, r, &input) != nil {
			controlError(w, 400, "Invalid path request")
			return
		}
		host, err := strictTailIP(input.Host)
		if err != nil {
			controlError(w, 400, "A private Relay address is required")
			return
		}
		// Only peers this app already routes to can be measured. This endpoint is a
		// status display, not a network scanner. The lock is released before pinging.
		h.mu.Lock()
		network := h.network
		known := !h.closed && h.status.Phase == "ready" && h.routes[input.Host] != nil
		h.mu.Unlock()
		if !known || network == nil {
			controlError(w, 409, "Open the private Relay route first")
			return
		}
		info, err := probePath(r.Context(), network, host)
		if err != nil {
			// An unreachable peer is a normal state to display, not a control failure.
			jsonResponse(w, 200, pathInfo{Kind: pathUnknown})
			return
		}
		jsonResponse(w, 200, info)
	case r.URL.Path == "/host" && r.Method == http.MethodPost:
		var input struct {
			PeerPort     uint16 `json:"peer_port"`
			IngressToken string `json:"ingress_token"`
			ServerID     string `json:"server_id"`
			MediaUDPPort uint16 `json:"media_udp_port"`
			VoiceUDPPort uint16 `json:"voice_udp_port,omitempty"`
		}
		if decodeControl(w, r, &input) != nil || input.PeerPort == 0 || input.PeerPort == 4580 || input.MediaUDPPort == 0 || len(input.IngressToken) < 32 || len(input.IngressToken) > 256 || !uuidPattern.MatchString(input.ServerID) {
			controlError(w, 400, "Initialized private peer listener required")
			return
		}
		if input.VoiceUDPPort == 0 {
			input.VoiceUDPPort = 4581
		}
		if input.VoiceUDPPort == input.MediaUDPPort {
			controlError(w, 400, "Voice and video need separate private ports")
			return
		}
		h.mu.Lock()
		defer h.mu.Unlock()
		if h.closed || h.status.Phase != "ready" {
			controlError(w, 409, "Connect the private network first")
			return
		}
		if h.host != nil {
			controlError(w, 409, "This network node already hosts a community")
			return
		}
		gateway, err := newHostGateway(h.network, input.PeerPort, input.IngressToken)
		if err != nil {
			controlError(w, 502, "Private host listener unavailable")
			return
		}
		voice, err := NewHostUDPRelay(h.network, input.VoiceUDPPort, maxVoiceDatagram)
		if err != nil {
			_ = gateway.Close()
			controlError(w, 502, "Private host voice unavailable")
			return
		}
		if err = h.media.StartHost(input.MediaUDPPort); err != nil {
			_ = gateway.Close()
			_ = voice.Close()
			controlError(w, 502, "Private host media unavailable")
			return
		}
		h.host = gateway
		h.hostVoice = voice
		h.serverID = input.ServerID
		hostURL := "http://" + h.status.OwnIP + ":4580"
		jsonResponse(w, 200, map[string]any{"address": hostURL, "http_url": hostURL, "server_id": h.serverID})
	case r.URL.Path == "/unhost" && r.Method == http.MethodPost:
		var input struct {
			ServerID string `json:"server_id"`
		}
		if decodeControl(w, r, &input) != nil || !uuidPattern.MatchString(input.ServerID) {
			controlError(w, 400, "Hosted community identity required")
			return
		}
		h.mu.Lock()
		defer h.mu.Unlock()
		if h.closed || h.host == nil || !strings.EqualFold(input.ServerID, h.serverID) {
			controlError(w, 409, "This network node does not host that community")
			return
		}
		// Serialize teardown with /host. Only this community's ingress listeners
		// are released; outgoing routes, TURN allocations and SDK login survive.
		serverID := h.serverID
		_ = h.host.Close()
		if h.hostVoice != nil {
			_ = h.hostVoice.Close()
		}
		_ = h.media.StopHost()
		h.host = nil
		h.hostVoice = nil
		h.serverID = ""
		jsonResponse(w, 200, map[string]any{"unhosted": true, "server_id": serverID})
	case r.URL.Path == "/shutdown" && r.Method == http.MethodPost:
		jsonResponse(w, 200, map[string]bool{"stopped": true})
		go h.close()
	default:
		controlError(w, 404, "Not available")
	}
}

var uuidPattern = regexp.MustCompile(`\A[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\z`)
var authLinkPattern = regexp.MustCompile(`https://[^\s<>"']+`)

func checkLocalStatePath(path string) (string, error) {
	// Rust canonicalizes Windows paths using this verbatim local-drive prefix.
	// Normalize only the two explicitly permitted drive forms, never UNC/device
	// namespaces or an arbitrary extended path supplied by a caller.
	if len(path) >= 7 && path[:4] == `\\?\` && (strings.EqualFold(path[4:7], `C:\`) || strings.EqualFold(path[4:7], `D:\`)) {
		path = path[4:]
	}
	full, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	if len(full) < 4 || (strings.ToUpper(full[:3]) != "C:\\" && strings.ToUpper(full[:3]) != "D:\\") {
		return "", errors.New("network state must stay on local C: or D: storage")
	}
	root, err := windows.UTF16PtrFromString(full[:3])
	if err != nil {
		return "", err
	}
	if windows.GetDriveType(root) == windows.DRIVE_REMOTE {
		return "", errors.New("network state must use local storage")
	}
	if len(full) == 3 {
		return "", errors.New("use a dedicated private network state folder")
	}
	for _, protected := range []string{os.Getenv("USERPROFILE"), os.Getenv("LOCALAPPDATA"), os.Getenv("APPDATA"), os.Getenv("SystemRoot"), os.Getenv("ProgramFiles")} {
		if protected != "" && strings.EqualFold(filepath.Clean(full), filepath.Clean(protected)) {
			return "", errors.New("use a dedicated private network state folder")
		}
	}
	paths := []string{}
	for current := full; ; {
		paths = append(paths, current)
		next := filepath.Dir(current)
		if next == current {
			break
		}
		current = next
	}
	for i := len(paths) - 1; i >= 0; i-- {
		info, err := os.Lstat(paths[i])
		if os.IsNotExist(err) {
			continue
		}
		if err != nil {
			return "", err
		}
		reparse := false
		if attributes, ok := info.Sys().(*syscall.Win32FileAttributeData); ok {
			reparse = attributes.FileAttributes&windows.FILE_ATTRIBUTE_REPARSE_POINT != 0
		}
		if info.Mode()&os.ModeSymlink != 0 || reparse {
			return "", errors.New("network state cannot traverse redirecting paths")
		}
	}
	return full, nil
}

func protectStateDirectory(path string) error {
	full, err := checkLocalStatePath(path)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(full, 0700); err != nil {
		return err
	}
	var token windows.Token
	if err = windows.OpenProcessToken(windows.CurrentProcess(), windows.TOKEN_QUERY, &token); err != nil {
		return err
	}
	defer token.Close()
	user, err := token.GetTokenUser()
	if err != nil {
		return err
	}
	descriptor, err := windows.SecurityDescriptorFromString("D:P(A;OICI;FA;;;" + user.User.Sid.String() + ")(A;OICI;FA;;;SY)")
	if err != nil {
		return err
	}
	acl, _, err := descriptor.DACL()
	if err != nil {
		return err
	}
	return windows.SetNamedSecurityInfo(full, windows.SE_FILE_OBJECT, windows.DACL_SECURITY_INFORMATION|windows.PROTECTED_DACL_SECURITY_INFORMATION, nil, nil, acl, nil)
}

func main() {
	stateDir := flag.String("state-dir", "", "Dedicated private per-user embedded network state")
	hostname := flag.String("hostname", "relay", "Embedded Relay node name")
	flag.Parse()
	token := os.Getenv("RELAY_NETWORK_CONTROL_TOKEN")
	if len(token) < 32 || len(token) > 256 {
		fmt.Fprintln(os.Stderr, "Private app control token is required")
		os.Exit(2)
	}
	state, err := checkLocalStatePath(*stateDir)
	if err != nil || *stateDir == "" {
		fmt.Fprintln(os.Stderr, "Choose a dedicated private network state folder on local C: or D:")
		os.Exit(2)
	}
	// Apply only to this helper process. No ambient credentials can enroll it
	// silently, force a re-login, select another control plane or an exit node.
	for _, name := range []string{"TS_AUTHKEY", "TS_AUTH_KEY", "TS_CLIENT_SECRET", "TS_CLIENT_ID", "TS_ID_TOKEN", "TS_AUDIENCE", "TSNET_FORCE_LOGIN", "TS_CONTROL_URL"} {
		envknob.Setenv(name, "")
	}
	envknob.Setenv("TS_NO_LOGS_NO_SUPPORT", "true")
	service := newHelper(token, func(onAuth func(string)) relayNetwork {
		server := &tsnet.Server{Dir: state, Hostname: *hostname, ControlURL: "https://controlplane.tailscale.com", RunWebClient: false, Logf: func(string, ...any) {}, UserLogf: func(format string, args ...any) {
			message := fmt.Sprintf(format, args...)
			for _, link := range authLinkPattern.FindAllString(message, 1) {
				onAuth(link)
			}
		}}
		return &sdkNetwork{server: server, stateDir: state}
	})
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		fmt.Fprintln(os.Stderr, "Private app control listener unavailable")
		os.Exit(2)
	}
	server := &http.Server{Handler: service, ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 15 * time.Second, WriteTimeout: 20 * time.Second, MaxHeaderBytes: 16 * 1024}
	// Startup output contains no credentials, login links or network node keys.
	_ = json.NewEncoder(os.Stdout).Encode(map[string]any{"version": 1, "pid": os.Getpid(), "control_url": "http://" + listener.Addr().String(), "phase": "idle"})
	go func() { _ = server.Serve(listener); service.close() }()
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt)
	select {
	case <-service.stop:
	case <-signals:
		service.close()
	}
	_ = server.Close()
	_ = listener.Close()
}
