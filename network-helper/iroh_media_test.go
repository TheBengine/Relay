package main

import (
	"bytes"
	"encoding/binary"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestIrohMediaFixedDestinationAndLocalRoundTrip(t *testing.T) {
	server, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close()
	provider, err := newIrohMediaProvider("", "100.127.255.254", uint16(server.LocalAddr().(*net.UDPAddr).Port))
	if err != nil {
		t.Fatal(err)
	}
	own, _ := provider.TailscaleIPs()
	packet, err := provider.ListenPacket("udp4", net.JoinHostPort(own.String(), "0"))
	if err != nil {
		t.Fatal(err)
	}
	defer packet.Close()
	_ = packet.SetDeadline(time.Now().Add(2 * time.Second))
	_ = server.SetDeadline(time.Now().Add(2 * time.Second))
	peer := &net.UDPAddr{IP: net.ParseIP("100.127.255.254"), Port: server.LocalAddr().(*net.UDPAddr).Port}
	payload := bytes.Repeat([]byte{0x81}, 1600)
	if _, err = packet.WriteTo(payload, peer); err != nil {
		t.Fatal(err)
	}
	buffer := make([]byte, 1601)
	n, from, err := server.ReadFromUDP(buffer)
	if err != nil || !bytes.Equal(buffer[:n], payload) {
		t.Fatal("outgoing media", n, err)
	}
	_, _ = server.WriteToUDP(payload, from)
	n, remote, err := packet.ReadFrom(buffer)
	if err != nil || !bytes.Equal(buffer[:n], payload) || remote.String() != peer.String() {
		t.Fatal("incoming media", n, remote, err)
	}
	if _, err = packet.WriteTo(payload, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: peer.Port}); err == nil {
		t.Fatal("loopback destination accepted from peer")
	}
	if _, err = packet.WriteTo(append(payload, 0), peer); err == nil {
		t.Fatal("oversized packet accepted")
	}
	for _, gateway := range []string{"http://example.com:1234", "http://127.0.0.1:1234/private", "http://u@127.0.0.1:1234", "http://127.0.0.1:0"} {
		if _, err := newIrohMediaProvider(gateway, "100.127.255.254", 7882); err == nil {
			t.Fatal("unsafe gateway", gateway)
		}
	}
}

func TestIrohMediaFramedStreamAndControlWithoutNetworkLogin(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/_relay/media" || r.Header.Get("Upgrade") != "relay-media-v1" {
			http.Error(w, "bad", 400)
			return
		}
		conn, buffer, err := w.(http.Hijacker).Hijack()
		if err != nil {
			return
		}
		defer conn.Close()
		_, _ = buffer.WriteString("HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: relay-media-v1\r\n\r\n")
		_ = buffer.Flush()
		var header [2]byte
		if _, err = io.ReadFull(buffer, header[:]); err != nil {
			return
		}
		payload := make([]byte, int(binary.BigEndian.Uint16(header[:])))
		if _, err = io.ReadFull(buffer, payload); err != nil {
			return
		}
		_, _ = conn.Write(append(header[:], payload...))
	}))
	defer server.Close()
	provider, err := newIrohMediaProvider(server.URL, "100.127.255.254", 7882)
	if err != nil {
		t.Fatal(err)
	}
	own, _ := provider.TailscaleIPs()
	packet, err := provider.ListenPacket("udp4", net.JoinHostPort(own.String(), "0"))
	if err != nil {
		t.Fatal(err)
	}
	defer packet.Close()
	_ = packet.SetDeadline(time.Now().Add(2 * time.Second))
	peer := &net.UDPAddr{IP: net.ParseIP("100.127.255.254"), Port: 7882}
	if _, err = packet.WriteTo([]byte("generated-video"), peer); err != nil {
		t.Fatal(err)
	}
	buffer := make([]byte, 1600)
	n, _, err := packet.ReadFrom(buffer)
	if err != nil || string(buffer[:n]) != "generated-video" {
		t.Fatal(n, err)
	}
	calls := 0
	h := newHelper(strings.Repeat("a", 64), func(func(string)) relayNetwork { calls++; return newFakeRelayNetwork() })
	defer h.close()
	response := controlCall(t, h, "POST", "/iroh-media", `{"gateway":"","host":"100.127.255.254","port":7882}`)
	if response.Code != 200 || calls != 0 || !strings.Contains(response.Body.String(), "turn:127.0.0.1:") {
		t.Fatal(response.Code, response.Body.String(), calls)
	}
	request := httptest.NewRequest("POST", "/iroh-media", strings.NewReader(`{}`))
	request.Header.Set("Authorization", "Bearer wrong")
	denied := httptest.NewRecorder()
	h.ServeHTTP(denied, request)
	if denied.Code != 403 {
		t.Fatal("control secret bypassed", denied.Code)
	}
}
