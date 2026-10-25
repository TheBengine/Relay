package main

import (
	"bytes"
	"encoding/json"
	"github.com/coder/websocket"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestHTTPSMediaTLSFramingAndNoRedirectOrDestinationEscape(t *testing.T) {
	const token = "owned.room.ticket"
	received := make(chan string, 8)
	server := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.ProtoMajor != 1 || r.Header.Get("Authorization") != "Bearer "+token || r.URL.Path != "/api/rtc/media" || r.URL.RawQuery != "" {
			http.Error(w, "denied", 401)
			return
		}
		received <- r.URL.String()
		ws, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer ws.CloseNow()
		ws.SetReadLimit(1602)
		for {
			kind, body, err := ws.Read(r.Context())
			if err != nil {
				return
			}
			if err = ws.Write(r.Context(), kind, body); err != nil {
				return
			}
		}
	}))
	server.EnableHTTP2 = true
	server.StartTLS()
	defer server.Close()
	endpoint := strings.Replace(server.URL, "https://", "wss://", 1) + "/api/rtc/media"
	provider, err := newHTTPSMediaProvider(endpoint, token, 7882)
	if err != nil {
		t.Fatal(err)
	}
	defer provider.cancel()
	own, _ := provider.TailscaleIPs()
	if packet, err := provider.ListenPacket("udp4", net.JoinHostPort(own.String(), "0")); err == nil {
		packet.Close()
		t.Fatal("untrusted TLS certificate accepted")
	}
	provider.transport.TLSClientConfig.RootCAs = server.Client().Transport.(*http.Transport).TLSClientConfig.RootCAs
	packet, err := provider.ListenPacket("udp4", net.JoinHostPort(own.String(), "0"))
	if err != nil {
		t.Fatal(err)
	}
	defer packet.Close()
	_ = packet.SetDeadline(time.Now().Add(2 * time.Second))
	peer := &net.UDPAddr{IP: net.ParseIP("100.127.255.254"), Port: 7882}
	payload := bytes.Repeat([]byte{0x81}, 1600)
	if _, err = packet.WriteTo(payload, peer); err != nil {
		t.Fatal(err)
	}
	got := make([]byte, 1601)
	n, from, err := packet.ReadFrom(got)
	if err != nil || !bytes.Equal(got[:n], payload) || from.String() != peer.String() {
		t.Fatal("media frame changed", n, from, err)
	}
	if _, err = packet.WriteTo(payload, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: 7882}); err == nil {
		t.Fatal("peer changed fixed SFU destination")
	}
	if _, err = packet.WriteTo(append(payload, 0), peer); err == nil {
		t.Fatal("oversized media frame accepted")
	}
	if path := <-received; path != "/api/rtc/media" {
		t.Fatal("ticket appeared in request URL")
	}
	// Even another trusted TLS endpoint must not receive this bearer on redirect.
	redirected := false
	sink := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { redirected = true; http.Error(w, "no", 401) }))
	defer sink.Close()
	redirect := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { http.Redirect(w, r, sink.URL+"/api/rtc/media", 307) }))
	defer redirect.Close()
	rejected, _ := newHTTPSMediaProvider(strings.Replace(redirect.URL, "https://", "wss://", 1)+"/api/rtc/media", token, 7882)
	defer rejected.cancel()
	rejected.transport.TLSClientConfig.RootCAs = redirect.Client().Transport.(*http.Transport).TLSClientConfig.RootCAs
	address, _ := rejected.TailscaleIPs()
	if other, err := rejected.ListenPacket("udp4", net.JoinHostPort(address.String(), "0")); err == nil {
		other.Close()
		t.Fatal("redirect accepted")
	}
	if redirected {
		t.Fatal("ticket forwarded to redirected origin")
	}
	for _, bad := range []string{"ws://127.0.0.1:1234/api/rtc/media", endpoint + "?ticket=x", endpoint + "#x", strings.Replace(endpoint, "/api/rtc/media", "/rtc", 1)} {
		if p, err := newHTTPSMediaProvider(bad, token, 7882); err == nil {
			p.cancel()
			t.Fatal("invalid route accepted")
		}
	}
}

func TestHTTPSMediaControlIsBoundedScopedAndDoesNotStartVPN(t *testing.T) {
	calls := 0
	h := newHelper(strings.Repeat("a", 64), func(func(string)) relayNetwork { calls++; return newFakeRelayNetwork() })
	defer h.close()
	body := `{"url":"wss://owned.example/api/rtc/media","token":"owned.room.ticket","peer_port":7882}`
	ids := []string{}
	for i := 0; i < 8; i++ {
		reply := controlCall(t, h, "POST", "/https-media", body)
		if reply.Code != 200 {
			t.Fatal(reply.Code, reply.Body.String())
		}
		var data struct {
			ID   string     `json:"id"`
			TURN TURNConfig `json:"turn"`
		}
		if err := json.Unmarshal(reply.Body.Bytes(), &data); err != nil {
			t.Fatal(err)
		}
		if len(data.ID) != 43 || !strings.HasPrefix(data.TURN.URL, "turn:127.0.0.1:") {
			t.Fatal("invalid local lease")
		}
		ids = append(ids, data.ID)
	}
	if reply := controlCall(t, h, "POST", "/https-media", body); reply.Code != 429 {
		t.Fatal("route bound bypassed", reply.Code)
	}
	if calls != 0 {
		t.Fatal("HTTPS media started a VPN SDK")
	}
	unknown := strings.Repeat("z", 43)
	controlCall(t, h, "POST", "/close-https-media", `{"id":"`+unknown+`"}`)
	if len(h.httpsMedia) != 8 {
		t.Fatal("another lease was closed")
	}
	for i := 0; i < 2; i++ {
		if reply := controlCall(t, h, "POST", "/close-https-media", `{"id":"`+ids[0]+`"}`); reply.Code != 200 {
			t.Fatal(reply.Code)
		}
	}
	if len(h.httpsMedia) != 7 {
		t.Fatal("repeated close changed another lease")
	}
	if reply := controlCall(t, h, "POST", "/https-media", body); reply.Code != 200 {
		t.Fatal("released route slot not reusable")
	}
	request := httptest.NewRequest("POST", "/https-media", strings.NewReader(body))
	request.Header.Set("Authorization", "Bearer wrong")
	denied := httptest.NewRecorder()
	h.ServeHTTP(denied, request)
	if denied.Code != 403 {
		t.Fatal("control authorization bypassed")
	}
}

func TestIrohHTTPSMediaGatewayFramingAndIsolation(t *testing.T) {
	const token = "owned.private.ticket"
	const virtual = "ws://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.iroh:4580/api/rtc/media"
	seen := make(chan string, 4)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/rtc/media" || r.URL.RawQuery != "" || r.Header.Get("Authorization") != "Bearer "+token {
			http.Error(w, "denied", 401)
			return
		}
		seen <- r.URL.String()
		ws, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer ws.CloseNow()
		ws.SetReadLimit(1602)
		for {
			kind, body, err := ws.Read(r.Context())
			if err != nil {
				return
			}
			if ws.Write(r.Context(), kind, body) != nil {
				return
			}
		}
	}))
	defer server.Close()
	p, err := newIrohHTTPSMediaProvider(virtual, server.URL, token, 7882)
	if err != nil {
		t.Fatal(err)
	}
	defer p.cancel()
	own, _ := p.TailscaleIPs()
	packet, err := p.ListenPacket("udp4", net.JoinHostPort(own.String(), "0"))
	if err != nil {
		t.Fatal(err)
	}
	defer packet.Close()
	_ = packet.SetDeadline(time.Now().Add(2 * time.Second))
	payload := []byte("private framed media")
	if _, err = packet.WriteTo(payload, &net.UDPAddr{IP: net.ParseIP("100.127.255.254"), Port: 7882}); err != nil {
		t.Fatal(err)
	}
	buf := make([]byte, 1600)
	n, _, err := packet.ReadFrom(buf)
	if err != nil || !bytes.Equal(buf[:n], payload) {
		t.Fatal("private framing failed", err)
	}
	if <-seen != "/api/rtc/media" {
		t.Fatal("capability entered the URL")
	}
	for _, bad := range []string{"http://example.com:4580", "http://127.0.0.2:1234", "https://127.0.0.1:1234", server.URL + "/other", server.URL + "?token=x", "http://user:secret@127.0.0.1:1234"} {
		if accepted, err := newIrohHTTPSMediaProvider(virtual, bad, token, 7882); err == nil {
			accepted.cancel()
			t.Fatal("unowned gateway accepted", bad)
		}
	}
	for _, bad := range []string{strings.Replace(virtual, "ws://", "wss://", 1), strings.Replace(virtual, ".iroh:", ".example:", 1), virtual + "?ticket=x", strings.Replace(virtual, "/api/rtc/media", "/api/local", 1)} {
		if accepted, err := newIrohHTTPSMediaProvider(bad, server.URL, token, 7882); err == nil {
			accepted.cancel()
			t.Fatal("invalid private origin accepted")
		}
	}
	if accepted, err := newHTTPSMediaProvider(strings.Replace(server.URL, "http://", "ws://", 1)+"/api/rtc/media", token, 7882); err == nil {
		accepted.cancel()
		t.Fatal("ordinary HTTPS path accepted plaintext WS")
	}
	redirect := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { http.Redirect(w, r, server.URL+"/api/rtc/media", 307) }))
	defer redirect.Close()
	rejected, _ := newIrohHTTPSMediaProvider(virtual, redirect.URL, token, 7882)
	defer rejected.cancel()
	if other, err := rejected.ListenPacket("udp4", net.JoinHostPort(own.String(), "0")); err == nil {
		other.Close()
		t.Fatal("private media followed redirect")
	}
	select {
	case <-seen:
		t.Fatal("ticket reached redirected origin")
	default:
	}
	calls := 0
	h := newHelper(strings.Repeat("a", 64), func(func(string)) relayNetwork { calls++; return newFakeRelayNetwork() })
	defer h.close()
	ids := []string{}
	for i := 0; i < 2; i++ {
		body, _ := json.Marshal(map[string]any{"url": virtual, "gateway": server.URL, "token": token, "peer_port": 7882})
		reply := controlCall(t, h, "POST", "/iroh-https-media", string(body))
		if reply.Code != 200 {
			t.Fatal(reply.Code, reply.Body.String())
		}
		var value struct {
			ID string `json:"id"`
		}
		if json.Unmarshal(reply.Body.Bytes(), &value) != nil || value.ID == "" {
			t.Fatal("missing lease")
		}
		ids = append(ids, value.ID)
	}
	first, second := h.httpsMedia[ids[0]], h.httpsMedia[ids[1]]
	controlCall(t, h, "POST", "/close-https-media", `{"id":"`+ids[0]+`"}`)
	select {
	case <-first.provider.ctx.Done():
	default:
		t.Fatal("own lease was not closed")
	}
	select {
	case <-second.provider.ctx.Done():
		t.Fatal("another lease was cancelled")
	default:
	}
	if calls != 0 || len(h.httpsMedia) != 1 {
		t.Fatal("private media affected VPN or another lease")
	}
}
