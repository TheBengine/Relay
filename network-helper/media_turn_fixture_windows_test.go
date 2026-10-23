package main

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

// This opt-in fixture remains entirely offline: native clients use the actual
// SFU and local TURN, while the injected mesh runs only over loopback UDP.
func TestOwnedNativeRTCMeshFixture(t *testing.T) {
	fixturePath := os.Getenv("RELAY_NATIVE_FIXTURE_JSON")
	if fixturePath == "" {
		t.Skip("owned native RTC acceptance fixture was not requested")
	}
	fixturePath, err := checkLocalStatePath(fixturePath)
	if err != nil {
		t.Fatal(err)
	}
	directory := filepath.Dir(fixturePath)
	if err = protectStateDirectory(directory); err != nil {
		t.Fatal(err)
	}
	binary, err := checkLocalStatePath(os.Getenv("RELAY_NATIVE_FIXTURE_SFU"))
	if err != nil {
		t.Fatal("a literal local pinned SFU binary is required")
	}
	file, err := os.Open(binary)
	if err != nil {
		t.Fatal(err)
	}
	digest := sha256.New()
	_, hashErr := io.Copy(digest, file)
	_ = file.Close()
	if hashErr != nil || hex.EncodeToString(digest.Sum(nil)) != "951f9466cd4450b3c3c7f1a5830a05a9bb97cc11d93cf1be9ebcd3b47cc316d1" {
		t.Fatal("owned fixture requires the pinned official SFU binary")
	}
	apiHold, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	apiPort := apiHold.Addr().(*net.TCPAddr).Port
	udpHold, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		_ = apiHold.Close()
		t.Fatal(err)
	}
	udpPort := uint16(udpHold.LocalAddr().(*net.UDPAddr).Port)
	_ = apiHold.Close()
	_ = udpHold.Close()
	for _, port := range []int{apiPort, int(udpPort)} {
		if port == 4580 || port == 4581 || port == 7880 || port == 7881 || port == 7882 {
			t.Fatal("fixture cannot use the user's existing service ports")
		}
	}
	key, _ := randomMediaCredential()
	secret, _ := randomMediaCredential()
	fixtureID, _ := randomMediaCredential()
	if key == "" || secret == "" || fixtureID == "" {
		t.Fatal("fixture random identity creation failed")
	}
	room := "owned-" + fixtureID[:12]
	configPath := filepath.Join(directory, "livekit.yaml")
	config := fmt.Sprintf("port: %d\nbind_addresses: [\"127.0.0.1\"]\nrtc:\n  tcp_port: 0\n  udp_port: %d\n  use_external_ip: false\n  use_ice_lite: true\n  node_ip: 100.80.99.1\n  enable_loopback_candidate: true\n  ips:\n    includes: [\"127.0.0.0/8\"]\nkeys:\n  %s: %s\nlogging:\n  level: debug\n  pion_level: debug\n", apiPort, udpPort, key, secret)
	if err = os.WriteFile(configPath, []byte(config), 0600); err != nil {
		t.Fatal(err)
	}
	nativePassed, ownedStopped := false, false
	helperEvidence := map[string]any{}
	t.Cleanup(func() {
		_ = os.Remove(configPath)
		_ = os.Remove(fixturePath)
		result := "failed"
		if nativePassed && ownedStopped {
			result = "passed"
		}
		receipt, _ := json.MarshalIndent(map[string]any{
			"result": result, "scope": "owned-offline-native-RTC-over-fake-userspace-UDP",
			"native_acceptance_passed": nativePassed, "owned_processes_stopped": ownedStopped,
			"system_vpn_required": false, "external_provider_enrollment": false,
			"microphone_or_camera_used": false, "sfu_sha256": hex.EncodeToString(digest.Sum(nil)),
			"helper_transport_evidence": helperEvidence,
		}, "", "  ")
		_ = os.WriteFile(filepath.Join(directory, "bridge-receipt.json"), receipt, 0600)
	})
	stdout, err := os.Create(filepath.Join(directory, "sfu.stdout.log"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = stdout.Close() })
	stderr, err := os.Create(filepath.Join(directory, "sfu.stderr.log"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = stderr.Close() })
	command := exec.Command(binary, "--config", configPath)
	command.Dir, command.Stdout, command.Stderr = directory, stdout, stderr
	command.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x08000000}
	command.Env = []string{}
	for _, entry := range os.Environ() {
		if !strings.HasPrefix(entry, "RELAY_") {
			command.Env = append(command.Env, entry)
		}
	}
	if err = command.Start(); err != nil {
		t.Fatal(err)
	}
	stopped := make(chan error, 1)
	go func() { stopped <- command.Wait() }()
	t.Cleanup(func() {
		select {
		case <-stopped:
			ownedStopped = true
		default:
			_ = command.Process.Kill()
			select {
			case <-stopped:
				ownedStopped = true
			case <-time.After(5 * time.Second):
				t.Error("owned SFU process did not stop")
			}
		}
	})
	client := &http.Client{Timeout: time.Second, Transport: &http.Transport{Proxy: nil}}
	ready := false
	for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); {
		response, err := client.Get(fmt.Sprintf("http://127.0.0.1:%d/", apiPort))
		if err == nil {
			_ = response.Body.Close()
			if response.StatusCode == 200 {
				ready = true
				break
			}
		}
		time.Sleep(100 * time.Millisecond)
	}
	if !ready {
		t.Fatal("owned SFU did not become ready")
	}
	mesh := newTestPacketMesh()
	host, err := NewHostUDPRelay(mesh.provider("100.80.99.1"), udpPort, maxMediaDatagram)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = host.Close() })
	nodes := make(map[string]map[string]any)
	mediaNodes := make(map[string]*MediaRelay)
	for index, name := range []string{"alice", "bob"} {
		ip := fmt.Sprintf("100.80.99.%d", index+2)
		provider := mesh.provider(ip)
		media, err := NewMediaRelay(provider)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = media.Close() })
		mediaNodes[name] = media
		turn, err := media.AddPeerPort(netip.MustParseAddr("100.80.99.1"), udpPort)
		if err != nil {
			t.Fatal(err)
		}
		nodes[name] = map[string]any{"url": fmt.Sprintf("ws://127.0.0.1:%d", apiPort), "token": fixtureJWT(key, secret, name, room),
			"turn": turn, "identity": name, "virtual_ip": ip}
	}
	t.Cleanup(func() {
		diagnostics := map[string]any{
			"alice_turn_allocations": mediaNodes["alice"].turn.server.AllocationCount(),
			"bob_turn_allocations":   mediaNodes["bob"].turn.server.AllocationCount(),
		}
		host.mu.Lock()
		diagnostics["host_loopback_bridge_count"] = len(host.bridges)
		host.mu.Unlock()
		mesh.mu.Lock()
		counters := make(map[string]int, len(mesh.counters))
		for name, count := range mesh.counters {
			counters[name] = count
		}
		diagnostics["alice_virtual_allocation_creations"] = mesh.allocationCreations["100.80.99.2"]
		diagnostics["bob_virtual_allocation_creations"] = mesh.allocationCreations["100.80.99.3"]
		mesh.mu.Unlock()
		diagnostics["synthetic_mesh_packet_classes"] = counters
		encoded, _ := json.MarshalIndent(diagnostics, "", "  ")
		_ = os.WriteFile(filepath.Join(directory, "transport-diagnostics.json"), encoded, 0600)
	})
	fixture := map[string]any{"fixture_id": fixtureID, "transport": "owned-fake-userspace", "room": room,
		"server_id": "13a1ff85-9231-4d45-9b8c-a00000000001", "host_virtual_ip": "100.80.99.1", "media_udp_port": udpPort,
		"alice": nodes["alice"], "bob": nodes["bob"], "evidence_filename": fixturePath + ".result.json"}
	encoded, err := json.MarshalIndent(fixture, "", "  ")
	if err != nil || os.WriteFile(fixturePath, encoded, 0600) != nil {
		t.Fatal("owned RTC fixture configuration could not be written")
	}
	t.Log("Owned offline native RTC fixture ready; waiting for the explicit native test result")
	for deadline := time.Now().Add(240 * time.Second); time.Now().Before(deadline); {
		if result, err := os.ReadFile(fixturePath + ".result.json"); err == nil {
			if len(result) > 65536 {
				t.Fatal("native fixture result exceeded its bound")
			}
			var evidence map[string]any
			if json.Unmarshal(result, &evidence) != nil || evidence["result"] != "passed" {
				t.Fatal("native RTC acceptance did not pass")
			}
			mesh.mu.Lock()
			aliceAllocations := mesh.allocationCreations["100.80.99.2"]
			bobAllocations := mesh.allocationCreations["100.80.99.3"]
			aliceDTLS := mesh.counters["100.80.99.2:dtls"]
			bobDTLS := mesh.counters["100.80.99.3:dtls"]
			aliceRTP := mesh.counters["100.80.99.2:rtp_or_rtcp"]
			bobRTP := mesh.counters["100.80.99.3:rtp_or_rtcp"]
			mesh.mu.Unlock()
			host.mu.Lock()
			bridges := len(host.bridges)
			host.mu.Unlock()
			helperEvidence = map[string]any{
				"alice_virtual_allocation_creations":     aliceAllocations,
				"bob_virtual_allocation_creations":       bobAllocations,
				"alice_successful_dtls_datagrams":        aliceDTLS,
				"bob_successful_dtls_datagrams":          bobDTLS,
				"alice_successful_rtp_or_rtcp_datagrams": aliceRTP,
				"bob_successful_rtp_or_rtcp_datagrams":   bobRTP,
				"isolated_host_loopback_bridges":         bridges,
			}
			if aliceAllocations == 0 || bobAllocations == 0 || aliceDTLS == 0 || bobDTLS == 0 || aliceRTP == 0 || bobRTP == 0 || bridges != 2 {
				t.Fatal("native frames lacked matching per-client virtual TURN allocation and UDP bridge evidence")
			}
			nativePassed = true
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
	t.Fatal("owned native RTC acceptance fixture timed out")
}

func fixtureJWT(key, secret, identity, room string) string {
	encode := base64.RawURLEncoding.EncodeToString
	header := encode([]byte(`{"alg":"HS256","typ":"JWT"}`))
	now := time.Now().Unix()
	payload, _ := json.Marshal(map[string]any{"iss": key, "sub": identity, "nbf": now - 5, "exp": now + 600,
		"video": map[string]any{"roomJoin": true, "room": room, "canPublish": true, "canSubscribe": true, "canPublishData": true}})
	body := header + "." + encode(payload)
	mac := hmac.New(sha256.New, []byte(secret))
	_, _ = mac.Write([]byte(body))
	return body + "." + encode(mac.Sum(nil))
}
