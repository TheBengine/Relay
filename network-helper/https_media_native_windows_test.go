package main

// Explicit, device-free transport acceptance: an owned SFU, generated native
// peers and this provider. Room authorization is exercised by the server tests.
import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func TestHTTPSMediaGeneratedNative(t *testing.T) {
	runGeneratedNativeBridge(t, false)
}
func TestIrohLoopbackMediaGeneratedNative(t *testing.T) {
	runGeneratedNativeBridge(t, true)
}
func runGeneratedNativeBridge(t *testing.T, private bool) {
	native, sfu := os.Getenv("RELAY_HTTPS_NATIVE_TEST_EXE"), os.Getenv("RELAY_HTTPS_NATIVE_SFU")
	if native == "" || sfu == "" {
		t.Skip("requires explicit owned native test binary and bundled SFU")
	}
	for _, item := range []struct{ path, hash string }{{native, os.Getenv("RELAY_HTTPS_NATIVE_TEST_SHA256")}, {sfu, os.Getenv("RELAY_HTTPS_NATIVE_SFU_SHA256")}} {
		if !filepath.IsAbs(item.path) || len(item.path) < 3 || !strings.Contains("CDcd", item.path[:1]) || item.path[1] != ':' {
			t.Fatal("direct C:/D: executable required")
		}
		for p := filepath.Clean(item.path); ; p = filepath.Dir(p) {
			info, err := os.Lstat(p)
			if err != nil || info.Mode()&os.ModeSymlink != 0 {
				t.Fatal("redirected executable path")
			}
			if attr, ok := info.Sys().(*syscall.Win32FileAttributeData); ok && attr.FileAttributes&syscall.FILE_ATTRIBUTE_REPARSE_POINT != 0 {
				t.Fatal("redirected executable path")
			}
			if filepath.Dir(p) == p {
				break
			}
		}
		raw, err := os.ReadFile(item.path)
		if err != nil {
			t.Fatal("owned executable unavailable")
		}
		sum := sha256.Sum256(raw)
		if len(item.hash) != 64 || hex.EncodeToString(sum[:]) != item.hash {
			t.Fatal("owned executable digest changed")
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 75*time.Second)
	defer cancel()
	tcpPort := func() int {
		l, e := net.Listen("tcp4", "127.0.0.1:0")
		if e != nil {
			t.Fatal(e)
		}
		defer l.Close()
		return l.Addr().(*net.TCPAddr).Port
	}
	apiPort, rtcTCP := tcpPort(), tcpPort()
	u, e := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if e != nil {
		t.Fatal(e)
	}
	udpPort := u.LocalAddr().(*net.UDPAddr).Port
	u.Close()
	credential := func() string {
		b := make([]byte, 32)
		if _, e := rand.Read(b); e != nil {
			t.Fatal(e)
		}
		return hex.EncodeToString(b)
	}
	key, secret := credential(), credential()
	config := fmt.Sprintf("port: %d\nbind_addresses: [\"127.0.0.1\"]\nrtc:\n  tcp_port: %d\n  udp_port: %d\n  use_external_ip: false\n  node_ip: 100.127.255.254\n  ips:\n    includes: [\"127.0.0.1/32\"]\n  enable_loopback_candidate: true\n  use_ice_lite: true\nkeys:\n  %s: %s\nlogging:\n  level: error\n", apiPort, rtcTCP, udpPort, key, secret)
	var sfuLog bytes.Buffer
	process := exec.CommandContext(ctx, sfu)
	process.Stdout = &sfuLog
	process.Stderr = &sfuLog
	process.Env = append(os.Environ(), "LIVEKIT_CONFIG="+config)
	process.SysProcAttr = &syscall.SysProcAttr{CreationFlags: 0x08000000}
	if e = process.Start(); e != nil {
		t.Fatal("owned SFU start failed")
	}
	defer func() { _ = process.Process.Kill(); _ = process.Wait() }()
	client := &http.Client{Transport: &http.Transport{Proxy: nil}, Timeout: time.Second}
	api := fmt.Sprintf("http://127.0.0.1:%d", apiPort)
	ready := false
	for until := time.Now().Add(8 * time.Second); time.Now().Before(until); {
		r, err := client.Get(api)
		if err == nil {
			r.Body.Close()
			ready = r.StatusCode == 200
		}
		if ready {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if !ready {
		t.Fatal("owned SFU readiness failed")
	}
	room := "owned-https-media-" + credential()[:16]
	tokens := make(map[string]bool)
	users := map[string]any{}
	for _, name := range []string{"alice", "bob", "eve"} {
		id := fmt.Sprintf("00000000-0000-4000-8000-%012d", len(users)+1)
		claims := map[string]any{"iss": key, "sub": id, "nbf": time.Now().Unix() - 1, "exp": time.Now().Unix() + 120, "video": map[string]any{"room": room, "roomJoin": true, "canSubscribe": true, "canPublish": true, "canPublishData": false}}
		payload, _ := json.Marshal(claims)
		raw := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"HS256","typ":"JWT"}`)) + "." + base64.RawURLEncoding.EncodeToString(payload)
		mac := hmac.New(sha256.New, []byte(secret))
		mac.Write([]byte(raw))
		token := raw + "." + base64.RawURLEncoding.EncodeToString(mac.Sum(nil))
		tokens[token] = true
		users[name] = map[string]any{"session": map[string]any{"user": map[string]any{"id": id}}, "ticket": token}
	}
	apiURL, _ := url.Parse(api)
	proxy := httputil.NewSingleHostReverseProxy(apiURL)
	signal := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != "GET" || (r.URL.Path != "/rtc" && r.URL.Path != "/rtc/v1" && r.URL.Path != "/rtc/v1/validate") || !tokens[strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")] {
			http.Error(w, "denied", 403)
			return
		}
		proxy.ServeHTTP(w, r)
	}))
	defer signal.Close()
	capability := "owned.media." + credential()
	var packets, mediaSockets, toUDP, fromUDP atomic.Uint64
	media := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/rtc/media" || r.URL.RawQuery != "" || r.Header.Get("Authorization") != "Bearer "+capability {
			http.Error(w, "denied", 403)
			return
		}
		ws, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer ws.CloseNow()
		mediaSockets.Add(1)
		ws.SetReadLimit(1602)
		udp, err := net.DialUDP("udp4", nil, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: udpPort})
		if err != nil {
			return
		}
		defer udp.Close()
		stream := websocket.NetConn(ctx, ws, websocket.MessageBinary)
		ws.SetReadLimit(1602)
		defer stream.Close()
		go func() {
			defer ws.CloseNow()
			buf := make([]byte, 1602)
			for {
				n, err := udp.Read(buf[2:])
				if err != nil {
					return
				}
				binary.BigEndian.PutUint16(buf, uint16(n))
				if _, err = stream.Write(buf[:n+2]); err != nil {
					return
				}
				packets.Add(1)
				fromUDP.Add(1)
			}
		}()
		for {
			var size [2]byte
			if _, err = io.ReadFull(stream, size[:]); err != nil {
				return
			}
			n := int(binary.BigEndian.Uint16(size[:]))
			if n < 1 || n > 1600 {
				return
			}
			packet := make([]byte, n)
			if _, err = io.ReadFull(stream, packet); err != nil {
				return
			}
			if _, err = udp.Write(packet); err != nil {
				return
			}
			packets.Add(1)
			toUDP.Add(1)
		}
	}))
	if private {
		media.Start()
	} else {
		media.StartTLS()
	}
	defer media.Close()
	for _, name := range []string{"alice", "bob"} {
		var provider *httpsMediaProvider
		var err error
		if private {
			provider, err = newIrohHTTPSMediaProvider("ws://"+strings.Repeat("a", 64)+".iroh:4580/api/rtc/media", media.URL, capability, uint16(udpPort))
		} else {
			provider, err = newHTTPSMediaProvider(strings.Replace(media.URL, "https://", "wss://", 1)+"/api/rtc/media", capability, uint16(udpPort))
		}
		if err != nil {
			t.Fatal(err)
		}
		if !private {
			provider.transport.TLSClientConfig.RootCAs = media.Client().Transport.(*http.Transport).TLSClientConfig.RootCAs
		}
		turn, err := newLocalTurn(provider, func(ip net.IP) bool { return ip.Equal(net.ParseIP("100.127.255.254")) }, func(a *net.UDPAddr) bool { return a.Port == udpPort && a.IP.Equal(net.ParseIP("100.127.255.254")) })
		if err != nil {
			t.Fatal(err)
		}
		lease := &httpsMediaLease{turn: turn, provider: provider}
		defer lease.Close()
		users[name].(map[string]any)["turn"] = turn.Config
	}
	fixture := map[string]any{"schema": 1, "purpose": "relay-owned-https-native-v1", "room_id": room, "other_id": "owned-other", "users": users, "signaling": strings.Replace(signal.URL, "http://", "ws://", 1)}
	raw, _ := json.Marshal(fixture)
	file := filepath.Join(t.TempDir(), "private-quality-tickets.json")
	if err := os.WriteFile(file, raw, 0600); err != nil {
		t.Fatal(err)
	}
	cmd := exec.CommandContext(ctx, native, "rtc::people_tests::loopback_tests::actual_https_bridge_generated_media", "--exact", "--ignored", "--test-threads=1", "--nocapture")
	cmd.Env = append(os.Environ(), "RELAY_LOCAL_QUALITY_FIXTURE="+file)
	cmd.SysProcAttr = &syscall.SysProcAttr{CreationFlags: 0x08000000}
	out, err := cmd.CombinedOutput()
	for _, line := range strings.Split(string(out), "\n") {
		if strings.HasPrefix(line, "HTTPS_NATIVE_COUNTS ") {
			t.Log(line)
		}
	}
	if err != nil {
		t.Logf("owned media sockets=%d sent_to_sfu=%d received_from_sfu=%d", mediaSockets.Load(), toUDP.Load(), fromUDP.Load())
		_ = process.Process.Kill()
		_ = process.Wait()
		safe := string(out)
		for _, line := range strings.Split(sfuLog.String(), "\n") {
			parts := strings.SplitN(line, "\t", 6)
			if len(parts) >= 5 {
				safe += "\n" + strings.Join(parts[:5], "\t")
			}
		}
		for token := range tokens {
			safe = strings.ReplaceAll(safe, token, "<owned-token>")
		}
		for _, name := range []string{"alice", "bob"} {
			turn := users[name].(map[string]any)["turn"].(TURNConfig)
			for _, v := range []string{turn.Username, turn.Password} {
				safe = strings.ReplaceAll(safe, v, "<owned-turn-credential>")
			}
		}
		for _, v := range []string{capability, key, secret} {
			safe = strings.ReplaceAll(safe, v, "<owned-secret>")
		}
		if len(safe) > 8192 {
			safe = safe[:8192]
		}
		t.Log(safe)
		t.Fatalf("native generated-media trial failed (%v); no bearer output retained", err)
	}
	if !strings.Contains(string(out), "1 passed") || !strings.Contains(string(out), `"production_handover_probe":true`) || packets.Load() == 0 {
		t.Fatal("native test or HTTPS packet flow unproven")
	}
	t.Logf("owned HTTPS datagrams forwarded: %d", packets.Load())
}
