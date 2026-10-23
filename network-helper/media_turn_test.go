package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"net/netip"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/pion/logging"
	"github.com/pion/turn/v4"
)

// The test mesh uses real loopback UDP sockets. Only virtual addresses registered
// in this bus can be reached; no Tailscale enrollment or external traffic occurs.
type testPacketMesh struct {
	mu                  sync.Mutex
	virtual             map[string]*testMeshPacket
	actual              map[string]*testMeshPacket
	counters            map[string]int
	allocationCreations map[string]int
}

type testPacketProvider struct {
	mesh *testPacketMesh
	ip   netip.Addr
}

func newTestPacketMesh() *testPacketMesh {
	return &testPacketMesh{virtual: make(map[string]*testMeshPacket), actual: make(map[string]*testMeshPacket), counters: make(map[string]int), allocationCreations: make(map[string]int)}
}

func (m *testPacketMesh) provider(ip string) *testPacketProvider {
	return &testPacketProvider{mesh: m, ip: netip.MustParseAddr(ip)}
}

func (p *testPacketProvider) TailscaleIPs() (netip.Addr, netip.Addr) {
	return p.ip, netip.Addr{}
}

func (p *testPacketProvider) ListenPacket(network, address string) (net.PacketConn, error) {
	addr, err := net.ResolveUDPAddr("udp4", address)
	if err != nil || network != "udp4" || !addr.IP.Equal(net.IP(p.ip.AsSlice())) {
		return nil, errors.New("test mesh only binds the provider's virtual IPv4")
	}
	physical, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	isAllocation := addr.Port == 0
	if isAllocation {
		addr.Port = physical.LocalAddr().(*net.UDPAddr).Port
	}
	packet := &testMeshPacket{PacketConn: physical, provider: p, virtual: addr}
	p.mesh.mu.Lock()
	defer p.mesh.mu.Unlock()
	if p.mesh.virtual[addr.String()] != nil {
		_ = physical.Close()
		return nil, errors.New("virtual UDP port is already bound")
	}
	p.mesh.virtual[addr.String()] = packet
	p.mesh.actual[physical.LocalAddr().String()] = packet
	if isAllocation {
		p.mesh.allocationCreations[p.ip.String()]++
	}
	return packet, nil
}

type testMeshPacket struct {
	net.PacketConn
	provider  *testPacketProvider
	virtual   *net.UDPAddr
	closeOnce sync.Once
}

func (p *testMeshPacket) LocalAddr() net.Addr { return p.virtual }

func (p *testMeshPacket) WriteTo(payload []byte, to net.Addr) (int, error) {
	p.provider.mesh.mu.Lock()
	destination := p.provider.mesh.virtual[to.String()]
	class := "other"
	if len(payload) >= 20 && binary.BigEndian.Uint32(payload[4:8]) == 0x2112a442 {
		switch binary.BigEndian.Uint16(payload[:2]) {
		case 0x0001:
			class = "stun_binding_request"
		case 0x0101:
			class = "stun_binding_success"
		default:
			class = "stun_other"
		}
	} else if len(payload) > 0 && payload[0] >= 20 && payload[0] <= 63 {
		class = "dtls"
	} else if len(payload) > 0 && payload[0]&0xc0 == 0x80 {
		class = "rtp_or_rtcp"
	}
	p.provider.mesh.mu.Unlock()
	if destination == nil {
		return 0, errors.New("unregistered test mesh destination")
	}
	n, err := p.PacketConn.WriteTo(payload, destination.PacketConn.LocalAddr())
	if err == nil && n == len(payload) {
		p.provider.mesh.mu.Lock()
		p.provider.mesh.counters[p.provider.ip.String()+":"+class]++
		p.provider.mesh.mu.Unlock()
	}
	return n, err
}

func (p *testMeshPacket) ReadFrom(payload []byte) (int, net.Addr, error) {
	n, actual, err := p.PacketConn.ReadFrom(payload)
	if err != nil {
		return n, nil, err
	}
	p.provider.mesh.mu.Lock()
	source := p.provider.mesh.actual[actual.String()]
	p.provider.mesh.mu.Unlock()
	if source == nil {
		return 0, nil, errors.New("unregistered test mesh source")
	}
	return n, source.virtual, nil
}

func (p *testMeshPacket) Close() error {
	p.closeOnce.Do(func() {
		_ = p.PacketConn.Close()
		p.provider.mesh.mu.Lock()
		delete(p.provider.mesh.virtual, p.virtual.String())
		delete(p.provider.mesh.actual, p.PacketConn.LocalAddr().String())
		p.provider.mesh.mu.Unlock()
	})
	return nil
}

func newTestTURNClient(t *testing.T, config TURNConfig) (*turn.Client, net.PacketConn) {
	return newTestTURNClientFrom(t, config, net.IPv4(127, 0, 0, 1))
}

func newTestTURNClientFrom(t *testing.T, config TURNConfig, source net.IP) (*turn.Client, net.PacketConn) {
	t.Helper()
	endpoint := strings.TrimSuffix(strings.TrimPrefix(config.URL, "turn:"), "?transport=tcp")
	remote, err := net.ResolveTCPAddr("tcp4", endpoint)
	if err != nil {
		t.Fatal(err)
	}
	tcp, err := net.DialTCP("tcp4", &net.TCPAddr{IP: source}, remote)
	if err != nil {
		t.Fatal(err)
	}
	local := turn.NewSTUNConn(tcp)
	logger := logging.NewDefaultLoggerFactory()
	logger.Writer = io.Discard
	client, err := turn.NewClient(&turn.ClientConfig{
		TURNServerAddr: endpoint, Username: config.Username, Password: config.Password,
		Conn: local, LoggerFactory: logger, RTO: 100 * time.Millisecond,
	})
	if err != nil {
		_ = local.Close()
		t.Fatal(err)
	}
	if err := client.Listen(); err != nil {
		client.Close()
		_ = local.Close()
		t.Fatal(err)
	}
	t.Cleanup(func() {
		client.Close()
		_ = local.Close()
	})
	return client, local
}

func TestOwnedInterfaceSourcePermissionAndLoopbackTCPTURNBoundary(t *testing.T) {
	localSources, err := localUDPSourceAddresses()
	if err != nil {
		t.Fatal(err)
	}
	for _, source := range []net.Addr{
		&net.UDPAddr{IP: net.ParseIP("192.0.2.1"), Port: 12345},
		&net.UDPAddr{IP: net.ParseIP("203.0.113.1"), Port: 12345},
		&net.UDPAddr{IP: net.ParseIP("192.168.1.77"), Port: 12345},
		&net.UDPAddr{IP: net.ParseIP("127.0.0.1"), Port: 0},
	} {
		if udp := source.(*net.UDPAddr); udp.IP.IsLoopback() {
			if ownedTURNSource(source, localSources) {
				t.Fatal("invalid local TURN source port admitted")
			}
		} else {
			// Fixed foreign-source examples remain unowned in this isolated map.
			if ownedTURNSource(source, map[netip.Addr]struct{}{}) {
				t.Fatal("an unowned network source was admitted")
			}
		}
	}
	var physical netip.Addr
	for address := range localSources {
		if !address.IsLoopback() && !address.IsLinkLocalUnicast() {
			physical = address
			break
		}
	}
	if !physical.IsValid() {
		t.Skip("no non-loopback local IPv4 interface is available")
	}
	mesh := newTestPacketMesh()
	manager, err := NewMediaRelay(mesh.provider("100.80.0.1"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = manager.Close() })
	config, err := manager.AddPeer(netip.MustParseAddr("100.80.0.2"))
	if err != nil {
		t.Fatal(err)
	}
	if !ownedTURNSource(&net.TCPAddr{IP: physical.AsSlice(), Port: 12345}, localSources) ||
		!ownedTURNSource(&net.UDPAddr{IP: physical.AsSlice(), Port: 12345}, localSources) {
		t.Fatal("an owned physical interface source was not permitted")
	}
	// Windows rejects explicit physical-source binds when the destination is127.
	// Native ICE acceptance separately proves its socket factory's local TCP leg.
	client, source := newTestTURNClient(t, config)
	if !source.LocalAddr().(*net.TCPAddr).IP.IsLoopback() || !ownedTURNSource(source.LocalAddr(), localSources) {
		t.Fatal("local TURN client did not remain on loopback")
	}
	allocation, err := client.Allocate()
	if err != nil {
		t.Fatal("local TCP source could not allocate through the loopback TURN listener")
	}
	t.Cleanup(func() { _ = allocation.Close() })
	if err := client.CreatePermission(&net.UDPAddr{IP: net.ParseIP("100.80.0.2"), Port: 7882}); err != nil {
		t.Fatal("native-style source could not grant its selected media peer")
	}
	if err := client.CreatePermission(&net.UDPAddr{IP: net.ParseIP("192.0.2.1"), Port: 7882}); err == nil {
		t.Fatal("native-style source bypassed the exact private media peer restriction")
	}
}

func TestLocalTURNVirtualAllocationAndEncryptedDatagramRoundTrip(t *testing.T) {
	mesh := newTestPacketMesh()
	clientProvider := mesh.provider("100.80.0.1")
	peerProvider := mesh.provider("100.80.0.2")
	manager, err := NewMediaRelay(clientProvider)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = manager.Close() })
	config, err := manager.AddPeer(peerProvider.ip)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(config.URL, "turn:127.0.0.1:") || len(config.Password) < 40 || config.Password == config.Username {
		t.Fatal("TURN must bind loopback and use independent strong ephemeral credentials")
	}
	peer, err := peerProvider.ListenPacket("udp4", "100.80.0.2:7882")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = peer.Close() })
	client, _ := newTestTURNClient(t, config)
	relayed, err := client.Allocate()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = relayed.Close() })
	allocation := relayed.LocalAddr().(*net.UDPAddr)
	if !allocation.IP.Equal(net.IP(clientProvider.ip.AsSlice())) || allocation.Port == 0 {
		t.Fatal("TURN did not advertise the userspace mesh allocation")
	}
	if err := client.CreatePermission(peer.LocalAddr()); err != nil {
		t.Fatal(err)
	}
	opaque := []byte{0xff, 0x10, 0x20, 0, 0xaa, 0x77}
	if _, err := relayed.WriteTo(opaque, peer.LocalAddr()); err != nil {
		t.Fatal(err)
	}
	buffer := make([]byte, 2048)
	_ = peer.SetReadDeadline(time.Now().Add(3 * time.Second))
	n, from, err := peer.ReadFrom(buffer)
	if err != nil || !bytes.Equal(buffer[:n], opaque) || from.String() != allocation.String() {
		t.Fatalf("outbound media was not preserved: %v", err)
	}
	if _, err := peer.WriteTo(buffer[:n], from); err != nil {
		t.Fatal(err)
	}
	_ = relayed.SetReadDeadline(time.Now().Add(3 * time.Second))
	n, from, err = relayed.ReadFrom(buffer)
	if err != nil || !bytes.Equal(buffer[:n], opaque) || from.String() != peer.LocalAddr().String() {
		t.Fatalf("return media was not preserved: %v", err)
	}
	if err := client.CreatePermission(&net.UDPAddr{IP: net.IPv4(192, 168, 1, 1), Port: 7882}); err == nil {
		t.Fatal("TURN admitted an arbitrary LAN destination")
	}
	manager.RemovePeer(peerProvider.ip)
	if manager.allowedPeer(peerProvider.ip.AsSlice()) {
		t.Fatal("revoked peer retained media permission")
	}
}

func TestMeshAllocationDestinationBoundaryQuotaAndCleanup(t *testing.T) {
	mesh := newTestPacketMesh()
	provider := mesh.provider("100.80.0.1")
	allowed := net.ParseIP("100.80.0.2")
	allocator := &meshRelayAllocator{
		provider: provider, allowedPeer: func(ip net.IP) bool { return ip.Equal(allowed) },
		allowedDestination: func(addr *net.UDPAddr) bool { return addr.Port == 7882 && addr.IP.Equal(allowed) },
		allocations:        make(map[*restrictedRelayPacket]struct{}),
	}
	var first net.PacketConn
	for i := 0; i < maxTURNAllocation; i++ {
		packet, _, err := allocator.AllocatePacketConn("udp", 0)
		if err != nil {
			t.Fatal(err)
		}
		if i == 0 {
			first = packet
		}
	}
	if _, _, err := allocator.AllocatePacketConn("udp", 0); err == nil {
		t.Fatal("allocation quota was exceeded")
	}
	for _, destination := range []*net.UDPAddr{
		{IP: allowed, Port: 4580}, {IP: net.ParseIP("100.80.0.3"), Port: 7882},
		{IP: net.ParseIP("127.0.0.1"), Port: 7882}, {IP: net.ParseIP("8.8.8.8"), Port: 7882},
	} {
		if _, err := first.WriteTo([]byte{1}, destination); err == nil {
			t.Fatal("non-media destination was admitted")
		}
	}
	if _, err := first.WriteTo(make([]byte, maxMediaDatagram+1), &net.UDPAddr{IP: allowed, Port: 7882}); err == nil {
		t.Fatal("oversized media datagram was admitted")
	}
	if _, _, err := allocator.AllocatePacketConn("udp6", 0); err == nil {
		t.Fatal("unsupported media family was admitted")
	}
	if _, _, err := allocator.AllocatePacketConn("udp", 4580); err == nil {
		t.Fatal("fixed relay port was admitted")
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	if _, _, err := allocator.AllocatePacketConn("udp", 0); err != nil {
		t.Fatal("closed allocation did not release capacity")
	}
	if err := allocator.close(); err != nil {
		t.Fatal(err)
	}
	if len(mesh.virtual) != 0 || len(allocator.allocations) != 0 {
		t.Fatal("media allocation sockets were left running")
	}
	if _, _, err := allocator.AllocatePacketConn("udp", 0); err == nil {
		t.Fatal("closed allocator admitted new sockets")
	}
}

func TestHostMediaBridgeKeepsRemotePeersSeparateAndStops(t *testing.T) {
	mesh := newTestPacketMesh()
	hostProvider := mesh.provider("100.80.0.1")
	manager, err := NewMediaRelay(hostProvider)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = manager.Close() })
	sfu, err := net.ListenPacket("udp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = sfu.Close() })
	mediaPort := uint16(sfu.LocalAddr().(*net.UDPAddr).Port)
	if err := manager.StartHost(mediaPort); err != nil {
		t.Fatal(err)
	}
	if err := manager.StartHost(7882); err == nil {
		t.Fatal("duplicate host listener was admitted")
	}
	peers := make([]net.PacketConn, 0, 2)
	sources := make([]string, 0, 2)
	buffer := make([]byte, 2048)
	for _, ip := range []string{"100.80.0.2", "100.80.0.3"} {
		peer, err := mesh.provider(ip).ListenPacket("udp4", ip+":0")
		if err != nil {
			t.Fatal(err)
		}
		peers = append(peers, peer)
		t.Cleanup(func() { _ = peer.Close() })
		payload := []byte(ip)
		if _, err := peer.WriteTo(payload, &net.UDPAddr{IP: net.ParseIP("100.80.0.1"), Port: int(mediaPort)}); err != nil {
			t.Fatal(err)
		}
		_ = sfu.SetReadDeadline(time.Now().Add(3 * time.Second))
		n, from, err := sfu.ReadFrom(buffer)
		if err != nil || !bytes.Equal(buffer[:n], payload) || !loopbackUDP(from) {
			t.Fatalf("SFU did not receive the private datagram: %v", err)
		}
		sources = append(sources, from.String())
		if _, err := sfu.WriteTo(buffer[:n], from); err != nil {
			t.Fatal(err)
		}
		_ = peer.SetReadDeadline(time.Now().Add(3 * time.Second))
		n, from, err = peer.ReadFrom(buffer)
		if err != nil || !bytes.Equal(buffer[:n], payload) || from.String() != (&net.UDPAddr{IP: net.ParseIP("100.80.0.1"), Port: int(mediaPort)}).String() {
			t.Fatalf("SFU reply reached the wrong virtual peer: %v", err)
		}
	}
	if sources[0] == sources[1] {
		t.Fatal("different virtual peers collapsed onto one SFU tuple")
	}
	if err := manager.Close(); err != nil && !errors.Is(err, net.ErrClosed) {
		t.Fatal(err)
	}
	manager.host.mu.Lock()
	count := len(manager.host.bridges)
	manager.host.mu.Unlock()
	if count != 0 {
		t.Fatal("owned media bridge sockets remained after shutdown")
	}
	if _, err := manager.AddPeer(netip.MustParseAddr("100.80.0.2")); !errors.Is(err, net.ErrClosed) {
		t.Fatal("closed media relay accepted a peer")
	}
}

func TestMediaPeerAllowlistRejectsNonPrivateAddresses(t *testing.T) {
	manager, err := NewMediaRelay(newTestPacketMesh().provider("100.80.0.1"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = manager.Close() })
	for _, ip := range []string{"127.0.0.1", "192.168.1.1", "8.8.8.8", "::1", "100.63.255.255", "100.128.0.0"} {
		if _, err := manager.AddPeer(netip.MustParseAddr(ip)); err == nil {
			t.Fatalf("invalid private media peer admitted: %s", ip)
		}
	}
	if _, err := NewLocalTurn(nil, func(net.IP) bool { return true }); err == nil {
		t.Fatal("missing network provider was admitted")
	}
	peer := netip.MustParseAddr("100.80.0.2")
	if _, err := manager.AddPeer(peer, 32768); err != nil {
		t.Fatal(err)
	}
	if !manager.allowedDestination(&net.UDPAddr{IP: peer.AsSlice(), Port: 32768}) || manager.allowedDestination(&net.UDPAddr{IP: peer.AsSlice(), Port: 7882}) {
		t.Fatal("media port permission does not match the configured SFU candidate port")
	}
	if _, err := manager.AddPeer(peer, 0); err == nil {
		t.Fatal("zero media port was admitted")
	}
}

func TestStopHostPreservesClientTURNAndAllowsOwnedRehost(t *testing.T) {
	mesh := newTestPacketMesh()
	manager, err := NewMediaRelay(mesh.provider("100.80.0.1"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = manager.Close() })
	peer := netip.MustParseAddr("100.80.0.2")
	config, err := manager.AddPeer(peer)
	if err != nil {
		t.Fatal(err)
	}
	allocation, _, err := manager.turn.allocator.AllocatePacketConn("udp", 0)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = allocation.Close() })
	if err := manager.StartHost(32768); err != nil {
		t.Fatal(err)
	}
	if err := manager.StopHost(); err != nil {
		t.Fatal(err)
	}
	if manager.host != nil || !manager.allowedPeer(peer.AsSlice()) || len(manager.turn.allocator.allocations) != 1 {
		t.Fatal("stopping the owned host also discarded client media state")
	}
	if next, err := manager.AddPeer(peer); err != nil || next != config {
		t.Fatal("client TURN credentials changed while stopping the host")
	}
	if err := manager.StartHost(32768); err != nil {
		t.Fatal("owned host UDP port did not become available for rehosting")
	}
	if err := manager.StopHost(); err != nil {
		t.Fatal(err)
	}
	if err := manager.StopHost(); err != nil {
		t.Fatal("stopping an already stopped host was not idempotent")
	}
}
