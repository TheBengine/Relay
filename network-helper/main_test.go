package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

func controlCall(t *testing.T, h *helper, method, path, body string) *httptest.ResponseRecorder {
	t.Helper()
	request := httptest.NewRequest(method, path, strings.NewReader(body))
	request.Header.Set("Authorization", "Bearer "+h.token)
	w := httptest.NewRecorder()
	h.ServeHTTP(w, request)
	return w
}

func TestHelperIdleDoesNotStartSDKAndControlsNeedSecret(t *testing.T) {
	calls := 0
	h := newHelper(strings.Repeat("a", 64), func(func(string)) relayNetwork { calls++; return newFakeRelayNetwork() })
	defer h.close()
	w := controlCall(t, h, "GET", "/status", "")
	if w.Code != 200 || !strings.Contains(w.Body.String(), `"phase":"idle"`) || calls != 0 {
		t.Fatal("idle started an SDK", w.Body.String())
	}
	for _, path := range []string{"/status", "/connect", "/route", "/host", "/unhost", "/shutdown"} {
		request := httptest.NewRequest("POST", path, strings.NewReader("{}"))
		request.Header.Set("Authorization", "Bearer wrong-secret")
		w = httptest.NewRecorder()
		h.ServeHTTP(w, request)
		if w.Code != 403 {
			t.Fatal(path, w.Code)
		}
	}
	request := httptest.NewRequest("GET", "/status", nil)
	request.Header.Set("Authorization", "Bearer "+h.token)
	request.Header["Origin"] = []string{""}
	w = httptest.NewRecorder()
	h.ServeHTTP(w, request)
	if w.Code != 403 {
		t.Fatal("browser Origin allowed")
	}
	if calls != 0 {
		t.Fatal("unauthorized control startedSDK")
	}
}

func TestUnhostRequiresMatchingCommunityPreservesRoutesAndAllowsRehost(t *testing.T) {
	clientBackend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "outgoing-route-still-alive")
	}))
	defer clientBackend.Close()
	firstPeer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "first-peer")
	}))
	defer firstPeer.Close()
	secondPeer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("X-Relay-Network-Peer") != "100.100.5.7" || r.Header.Get("X-Relay-Network-Ingress") != strings.Repeat("d", 64) {
			t.Error("rehost lost its exact ingress capability and private peer")
		}
		_, _ = io.WriteString(w, "second-peer")
	}))
	defer secondPeer.Close()
	peerPort := func(server *httptest.Server) uint16 {
		_, port, err := net.SplitHostPort(server.Listener.Addr().String())
		if err != nil {
			t.Fatal(err)
		}
		n, err := strconv.ParseUint(port, 10, 16)
		if err != nil {
			t.Fatal(err)
		}
		return uint16(n)
	}
	n := newFakeRelayNetwork()
	n.backend = strings.TrimPrefix(clientBackend.URL, "http://")
	h := newHelper(strings.Repeat("a", 64), nil)
	h.network = n
	h.status = helperStatus{Phase: "ready", OwnIP: "100.100.5.6"}
	media, err := NewMediaRelay(n)
	if err != nil {
		t.Fatal(err)
	}
	h.media = media
	defer h.close()
	const serverID = "7d47db98-4153-4a3b-9a5d-45924666b628"
	const otherID = "3b484a27-321d-4f2d-aeb5-f8314abf20d2"
	hostRequest := func(port uint16, secret string) string {
		return fmt.Sprintf(`{"peer_port":%d,"ingress_token":%q,"server_id":%q,"media_udp_port":32768,"voice_udp_port":32769}`, port, secret, serverID)
	}
	if w := controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580}`); w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	clientRoute := h.routes["100.100.5.7"]
	clientVoice := h.voices["100.100.5.7"]
	turnBefore, err := media.AddPeer(netip.MustParseAddr("100.100.5.7"))
	if err != nil {
		t.Fatal(err)
	}
	if w := controlCall(t, h, "POST", "/host", hostRequest(peerPort(firstPeer), strings.Repeat("c", 64))); w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	oldHost := h.host
	oldHostAddress := oldHost.listener.Addr().String()
	for _, body := range []string{`{"server_id":"bad"}`, `{"server_id":"` + serverID + `","unknown":true}`} {
		if w := controlCall(t, h, "POST", "/unhost", body); w.Code != 400 {
			t.Fatal("invalid unhost identity accepted", w.Code)
		}
	}
	if w := controlCall(t, h, "POST", "/unhost", `{"server_id":"`+otherID+`"}`); w.Code != 409 || h.host != oldHost {
		t.Fatal("wrong community affected current host", w.Code)
	}
	if w := controlCall(t, h, "POST", "/unhost", `{"server_id":"`+serverID+`"}`); w.Code != 200 || !strings.Contains(w.Body.String(), `"unhosted":true`) {
		t.Fatal(w.Code, w.Body.String())
	}
	if h.host != nil || h.hostVoice != nil || h.serverID != "" || h.closed || h.status.Phase != "ready" {
		t.Fatal("unhost did not clear only hosted state")
	}
	if conn, err := net.DialTimeout("tcp4", oldHostAddress, time.Second); err == nil {
		_ = conn.Close()
		t.Fatal("old hosted TCP gateway remained open")
	}
	for _, port := range []int{32768, 32769} {
		conn, err := n.ListenPacket("udp4", net.JoinHostPort("100.100.5.6", strconv.Itoa(port)))
		if err != nil {
			t.Fatal("old hosted UDP listener remained open", port, err)
		}
		_ = conn.Close()
	}
	if h.routes["100.100.5.7"] != clientRoute || h.voices["100.100.5.7"] != clientVoice {
		t.Fatal("unhost replaced outgoing route or active voice bridge")
	}
	response, err := http.Get(clientRoute.url + "/api/messages")
	if err != nil {
		t.Fatal("outgoing route was closed", err)
	}
	body, _ := io.ReadAll(response.Body)
	_ = response.Body.Close()
	if string(body) != "outgoing-route-still-alive" {
		t.Fatal("outgoing route changed", string(body))
	}
	turnAfter, err := media.AddPeer(netip.MustParseAddr("100.100.5.7"))
	if err != nil || turnAfter != turnBefore {
		t.Fatal("outgoing TURN was replaced", err)
	}
	n.mu.Lock()
	closed := n.closed
	n.mu.Unlock()
	if closed {
		t.Fatal("unhost stopped the SDK node")
	}
	if w := controlCall(t, h, "POST", "/unhost", `{"server_id":"`+serverID+`"}`); w.Code != 409 {
		t.Fatal("already-unhosted community incorrectly matched", w.Code)
	}
	if w := controlCall(t, h, "POST", "/host", hostRequest(peerPort(secondPeer), strings.Repeat("d", 64))); w.Code != 200 {
		t.Fatal("same SDK node could not rehost on a new peer listener", w.Code, w.Body.String())
	}
	request := httptest.NewRequest("GET", "/api/messages", nil)
	request.RemoteAddr = "100.100.5.7:49999"
	w := httptest.NewRecorder()
	h.host.server.Handler.ServeHTTP(w, request)
	if w.Code != 200 || w.Body.String() != "second-peer" {
		t.Fatal("rehost reused the stale Rust peer port", w.Code, w.Body.String())
	}
}

func TestExplicitConnectLoginStatusAndShutdownWithFakeSDK(t *testing.T) {
	n := newFakeRelayNetwork()
	n.gate = make(chan struct{})
	h := newHelper(strings.Repeat("a", 64), func(onAuth func(string)) relayNetwork { n.auth = onAuth; return n })
	defer h.close()
	if w := controlCall(t, h, "POST", "/connect", "{}"); w.Code != 200 {
		t.Fatal(w.Code)
	}
	deadline := time.Now().Add(time.Second)
	for {
		h.mu.Lock()
		status := h.status
		h.mu.Unlock()
		if status.Phase == "needs_login" {
			if status.AuthURL != "https://login.tailscale.com/a/fixture-only-auth" {
				t.Fatal(status)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("login status not delivered")
		}
		time.Sleep(time.Millisecond)
	}
	// Invalid auth URLs never become actionable login links.
	h.captureAuthURL("https://login.tailscale.com.evil/a/steal")
	close(n.gate)
	deadline = time.Now().Add(time.Second)
	for {
		h.mu.Lock()
		phase := h.status.Phase
		h.mu.Unlock()
		if phase == "ready" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("fake connection failed")
		}
		time.Sleep(time.Millisecond)
	}
	w := controlCall(t, h, "GET", "/status", "")
	var status helperStatus
	if json.Unmarshal(w.Body.Bytes(), &status) != nil || status.OwnIP != "100.100.5.6" || status.AuthURL != "" {
		t.Fatal(w.Body.String())
	}
	// More /connect calls never re-enroll or initialize another SDK.
	_ = controlCall(t, h, "POST", "/connect", "{}")
	n.mu.Lock()
	calls := n.connects
	n.mu.Unlock()
	if calls != 1 {
		t.Fatal(calls)
	}
	w = controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580}`)
	if w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	var route struct {
		HTTPURL      string     `json:"http_url"`
		Turn         TURNConfig `json:"turn"`
		VoiceUDPPort uint16     `json:"voice_udp_port"`
	}
	if json.Unmarshal(w.Body.Bytes(), &route) != nil || !strings.HasPrefix(route.HTTPURL, "http://127.0.0.1:") || route.Turn.Password == "" {
		t.Fatal(w.Body.String())
	}
	if route.VoiceUDPPort == 0 {
		t.Fatal("voice bridge port missing")
	}
	firstVoicePort := route.VoiceUDPPort
	w = controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580}`)
	var renewed struct {
		HTTPURL      string `json:"http_url"`
		VoiceUDPPort uint16 `json:"voice_udp_port"`
	}
	if json.Unmarshal(w.Body.Bytes(), &renewed) != nil || renewed.HTTPURL != route.HTTPURL || renewed.VoiceUDPPort == 0 || renewed.VoiceUDPPort == firstVoicePort {
		t.Fatal("voice session refresh did not replace UDP bridge while preserving HTTProute", w.Body.String())
	}
	if w = controlCall(t, h, "POST", "/route", `{"host":"8.8.8.8","port":4580}`); w.Code != 400 {
		t.Fatal("public target allowed")
	}
	if w = controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":22}`); w.Code != 400 {
		t.Fatal("arbitrary service allowed")
	}
	if w = controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580,"media_udp_port":0}`); w.Code != 400 {
		t.Fatal("zero media port allowed")
	}
	if w = controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580,"unknown":true}`); w.Code != 400 {
		t.Fatal("unknown control field allowed")
	}
	h.close()
	n.mu.Lock()
	closed := n.closed
	n.mu.Unlock()
	if !closed {
		t.Fatal("SDK node notclosed")
	}
	if w = controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580}`); w.Code != 409 {
		t.Fatal("closed helper createdroute")
	}
}

func TestLocalStatePathBoundaryBeforeAnySDKWrites(t *testing.T) {
	for _, path := range []string{`E:\must-not-read`, `\\server\share\must-not-read`, `\\?\GLOBALROOT\anything`, `C:\`} {
		if _, err := checkLocalStatePath(path); err == nil {
			t.Fatal("unsafe state path accepted", path)
		}
	}
	fixture := t.TempDir()
	canonical, err := checkLocalStatePath(`\\?\` + fixture)
	if err != nil || canonical != filepath.Clean(fixture) {
		t.Fatal("local Rust-canonical state path not accepted", canonical, err)
	}
}
