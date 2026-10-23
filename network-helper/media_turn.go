package main

import (
	"crypto/rand"
	"crypto/subtle"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"net"
	"net/netip"
	"strconv"
	"sync"
	"time"

	"github.com/pion/logging"
	"github.com/pion/turn/v4"
)

const (
	meshMediaPort     = 7882
	maxTURNAllocation = 12
	maxMediaPeers     = 16
	maxMediaBridges   = 256
	maxMediaDatagram  = 1600
	mediaBridgeIdle   = 90 * time.Second
)

var meshIPv4Range = netip.MustParsePrefix("100.64.0.0/10")

// NetPacketProvider is the app-owned userspace network, never the system VPN.
type NetPacketProvider interface {
	ListenPacket(network, address string) (net.PacketConn, error)
	TailscaleIPs() (netip.Addr, netip.Addr)
}

type TURNConfig struct {
	Address  string `json:"address,omitempty"`
	URL      string `json:"url"`
	Username string `json:"username"`
	Password string `json:"password"`
}

// LocalTurn gives native WebRTC an ordinary localhost socket, while its relay
// allocation lives inside tsnet. No virtual OS adapter or public TURN is needed.
type LocalTurn struct {
	Config    TURNConfig
	server    *turn.Server
	allocator *meshRelayAllocator
	closeOnce sync.Once
	closeErr  error
}

func NewLocalTurn(provider NetPacketProvider, allowedPeer func(net.IP) bool, mediaUDPPort ...uint16) (*LocalTurn, error) {
	port, err := selectedMediaPort(mediaUDPPort)
	if err != nil {
		return nil, err
	}
	return newLocalTurn(provider, allowedPeer, func(addr *net.UDPAddr) bool {
		return addr.Port == int(port) && allowedPeer != nil && allowedPeer(addr.IP)
	})
}

func selectedMediaPort(ports []uint16) (uint16, error) {
	if len(ports) == 0 {
		return meshMediaPort, nil
	}
	if len(ports) != 1 || ports[0] == 0 {
		return 0, errors.New("one nonzero private media port is required")
	}
	return ports[0], nil
}

func newLocalTurn(provider NetPacketProvider, allowedPeer func(net.IP) bool, allowedDestination func(*net.UDPAddr) bool) (*LocalTurn, error) {
	if provider == nil || allowedPeer == nil {
		return nil, errors.New("media network and peer permission are required")
	}
	localSources, err := localUDPSourceAddresses()
	if err != nil {
		return nil, err
	}
	username, err := randomMediaCredential()
	if err != nil {
		return nil, err
	}
	password, err := randomMediaCredential()
	if err != nil {
		return nil, err
	}
	physical, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		return nil, fmt.Errorf("open local media transport: %w", err)
	}
	local := trackListener(physical)
	allocator := &meshRelayAllocator{
		provider: provider, allowedPeer: allowedPeer, allowedDestination: allowedDestination,
		allocations: make(map[*restrictedRelayPacket]struct{}),
	}
	const realm = "relay-network"
	authKey := turn.GenerateAuthKey(username, realm, password)
	logger := logging.NewDefaultLoggerFactory()
	// Media details and local credentials must not reach helper logs.
	logger.Writer = io.Discard
	server, err := turn.NewServer(turn.ServerConfig{
		Realm: realm, LoggerFactory: logger, InboundMTU: maxMediaDatagram + 256,
		AuthHandler: func(user, requestedRealm string, source net.Addr) ([]byte, bool) {
			if !ownedTURNSource(source, localSources) || requestedRealm != realm ||
				subtle.ConstantTimeCompare([]byte(user), []byte(username)) != 1 {
				return nil, false
			}
			return authKey, true
		},
		ListenerConfigs: []turn.ListenerConfig{{
			Listener: local, RelayAddressGenerator: allocator,
			PermissionHandler: func(client net.Addr, peerIP net.IP) bool {
				return ownedTURNSource(client, localSources) && privateMediaIP(peerIP) && allowedPeer(peerIP)
			},
		}},
	})
	if err != nil {
		_ = local.Close()
		_ = allocator.close()
		return nil, fmt.Errorf("start local media transport: %w", err)
	}
	return &LocalTurn{
		Config: TURNConfig{
			URL:      "turn:" + local.Addr().String() + "?transport=tcp",
			Username: username, Password: password,
		},
		server: server, allocator: allocator,
	}, nil
}

func randomMediaCredential() (string, error) {
	var token [32]byte
	if _, err := rand.Read(token[:]); err != nil {
		return "", fmt.Errorf("create local media credential: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(token[:]), nil
}

func loopbackUDP(addr net.Addr) bool {
	udp, ok := addr.(*net.UDPAddr)
	return ok && udp.IP.IsLoopback()
}

func localUDPSourceAddresses() (map[netip.Addr]struct{}, error) {
	addresses, err := net.InterfaceAddrs()
	if err != nil {
		return nil, errors.New("local media interface information is unavailable")
	}
	local := make(map[netip.Addr]struct{})
	for _, address := range addresses {
		ip, ok := address.(*net.IPNet)
		if !ok {
			continue
		}
		addr, valid := netip.AddrFromSlice(ip.IP)
		addr = addr.Unmap()
		if valid && addr.Is4() && !addr.IsUnspecified() && !addr.IsMulticast() {
			local[addr] = struct{}{}
		}
	}
	return local, nil
}

// Native WebRTC binds TURN sockets to an interface address even when sending to
// localhost. The listener still binds only127; permit only sources this machine
// actually owns, with the same random TURN credentials and peer restrictions.
func ownedTURNSource(source net.Addr, local map[netip.Addr]struct{}) bool {
	var ip net.IP
	var port int
	switch source := source.(type) {
	case *net.UDPAddr:
		ip, port = source.IP, source.Port
	case *net.TCPAddr:
		ip, port = source.IP, source.Port
	default:
		return false
	}
	if port <= 0 || port > 65535 {
		return false
	}
	addr, valid := netip.AddrFromSlice(ip)
	addr = addr.Unmap()
	if !valid || !addr.Is4() {
		return false
	}
	_, own := local[addr]
	return addr.IsLoopback() || own
}

func privateMediaIP(ip net.IP) bool {
	addr, ok := netip.AddrFromSlice(ip)
	return ok && meshIPv4Range.Contains(addr.Unmap())
}

func (l *LocalTurn) Close() error {
	l.closeOnce.Do(func() {
		// Close the listener first so no new allocations race cleanup.
		l.closeErr = errors.Join(l.server.Close(), l.allocator.close())
	})
	return l.closeErr
}

type meshRelayAllocator struct {
	provider           NetPacketProvider
	allowedPeer        func(net.IP) bool
	allowedDestination func(*net.UDPAddr) bool
	mu                 sync.Mutex
	closed             bool
	pending            int
	allocations        map[*restrictedRelayPacket]struct{}
}

func (a *meshRelayAllocator) Validate() error {
	if a.provider == nil || a.allowedPeer == nil || a.allowedDestination == nil {
		return errors.New("media network is unavailable")
	}
	return nil
}

func (a *meshRelayAllocator) AllocatePacketConn(network string, requestedPort int) (net.PacketConn, net.Addr, error) {
	if network != "udp" && network != "udp4" || requestedPort != 0 {
		return nil, nil, errors.New("only ephemeral IPv4 media allocations are supported")
	}
	ownIP, _ := a.provider.TailscaleIPs()
	if !ownIP.Is4() || !meshIPv4Range.Contains(ownIP) {
		return nil, nil, errors.New("private network is not ready for media")
	}
	a.mu.Lock()
	if a.closed || len(a.allocations)+a.pending >= maxTURNAllocation {
		a.mu.Unlock()
		return nil, nil, errors.New("local media allocation limit reached")
	}
	a.pending++
	a.mu.Unlock()
	packet, err := a.provider.ListenPacket("udp4", net.JoinHostPort(ownIP.String(), "0"))
	a.mu.Lock()
	a.pending--
	if err != nil {
		a.mu.Unlock()
		return nil, nil, err
	}
	addr, ok := packet.LocalAddr().(*net.UDPAddr)
	if a.closed || !ok || !addr.IP.Equal(net.IP(ownIP.AsSlice())) || addr.Port <= 0 || addr.Port > 65535 {
		a.mu.Unlock()
		_ = packet.Close()
		return nil, nil, errors.New("invalid private media allocation")
	}
	restricted := &restrictedRelayPacket{PacketConn: packet, allocator: a}
	a.allocations[restricted] = struct{}{}
	a.mu.Unlock()
	return restricted, &net.UDPAddr{IP: append(net.IP(nil), addr.IP...), Port: addr.Port}, nil
}

func (a *meshRelayAllocator) AllocateConn(string, int) (net.Conn, net.Addr, error) {
	return nil, nil, errors.New("TCP media allocations are disabled")
}

func (a *meshRelayAllocator) close() error {
	a.mu.Lock()
	a.closed = true
	packets := make([]*restrictedRelayPacket, 0, len(a.allocations))
	for packet := range a.allocations {
		packets = append(packets, packet)
	}
	a.mu.Unlock()
	var closeErr error
	for _, packet := range packets {
		closeErr = errors.Join(closeErr, packet.Close())
	}
	return closeErr
}

// TURN permissions only restrict IPs. This additional wrapper restricts the
// port too, so local media credentials cannot reach another service on a host.
type restrictedRelayPacket struct {
	net.PacketConn
	allocator *meshRelayAllocator
	closeOnce sync.Once
	closeErr  error
}

func (p *restrictedRelayPacket) permitted(addr net.Addr) bool {
	udp, ok := addr.(*net.UDPAddr)
	return ok && privateMediaIP(udp.IP) && p.allocator.allowedDestination(udp)
}

func (p *restrictedRelayPacket) WriteTo(payload []byte, addr net.Addr) (int, error) {
	if len(payload) > maxMediaDatagram || !p.permitted(addr) {
		return 0, errors.New("media destination is not permitted")
	}
	return p.PacketConn.WriteTo(payload, addr)
}

func (p *restrictedRelayPacket) ReadFrom(payload []byte) (int, net.Addr, error) {
	for {
		n, from, err := p.PacketConn.ReadFrom(payload)
		if err != nil || n <= maxMediaDatagram && p.permitted(from) {
			return n, from, err
		}
	}
}

func (p *restrictedRelayPacket) Close() error {
	p.closeOnce.Do(func() {
		p.closeErr = p.PacketConn.Close()
		p.allocator.mu.Lock()
		delete(p.allocator.allocations, p)
		p.allocator.mu.Unlock()
	})
	return p.closeErr
}

// MediaRelay owns the TURN adapter and the SFU's userspace-network UDP bridge.
// Every remote tuple gets its own loopback socket; ICE cannot confuse peers.
type MediaRelay struct {
	provider  NetPacketProvider
	turn      *LocalTurn
	mu        sync.Mutex
	closed    bool
	peers     map[netip.Addr]uint16
	host      *HostUDPRelay
	closeOnce sync.Once
	closeErr  error
}

type mediaUDPBridge struct {
	conn *net.UDPConn
	last time.Time
}

func NewMediaRelay(provider NetPacketProvider) (*MediaRelay, error) {
	manager := &MediaRelay{
		provider: provider, peers: make(map[netip.Addr]uint16),
	}
	localTurn, err := newLocalTurn(provider, manager.allowedPeer, manager.allowedDestination)
	if err != nil {
		return nil, err
	}
	manager.turn = localTurn
	return manager, nil
}

func (m *MediaRelay) AddPeer(host netip.Addr, mediaUDPPort ...uint16) (TURNConfig, error) {
	port, err := selectedMediaPort(mediaUDPPort)
	if err != nil {
		return TURNConfig{}, err
	}
	if !host.Is4() || !meshIPv4Range.Contains(host) {
		return TURNConfig{}, errors.New("media requires a private Relay host address")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return TURNConfig{}, net.ErrClosed
	}
	if _, exists := m.peers[host]; !exists && len(m.peers) >= maxMediaPeers {
		return TURNConfig{}, errors.New("too many active private media hosts")
	}
	m.peers[host] = port
	return m.turn.Config, nil
}

func (m *MediaRelay) AddPeerPort(host netip.Addr, port uint16) (TURNConfig, error) {
	return m.AddPeer(host, port)
}

func (m *MediaRelay) RemovePeer(host netip.Addr) {
	m.mu.Lock()
	delete(m.peers, host)
	m.mu.Unlock()
}

func (m *MediaRelay) allowedPeer(peer net.IP) bool {
	addr, ok := netip.AddrFromSlice(peer)
	if !ok {
		return false
	}
	addr = addr.Unmap()
	m.mu.Lock()
	defer m.mu.Unlock()
	_, allowed := m.peers[addr]
	return !m.closed && allowed
}

func (m *MediaRelay) allowedDestination(peer *net.UDPAddr) bool {
	addr, ok := netip.AddrFromSlice(peer.IP)
	if !ok || peer.Port <= 0 || peer.Port > 65535 {
		return false
	}
	addr = addr.Unmap()
	m.mu.Lock()
	defer m.mu.Unlock()
	port, allowed := m.peers[addr]
	return !m.closed && allowed && int(port) == peer.Port
}

func (m *MediaRelay) StartHost(localUDPPort uint16) error {
	if localUDPPort == 0 {
		return errors.New("private video server port is required")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return net.ErrClosed
	}
	if m.host != nil {
		return errors.New("private media host is already running")
	}
	host, err := NewHostUDPRelay(m.provider, localUDPPort, maxMediaDatagram)
	if err != nil {
		return err
	}
	m.host = host
	return nil
}

func (m *MediaRelay) StopHost() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.host == nil {
		return nil
	}
	// Serialize stop/restart until the old virtual port is actually released.
	err := m.host.Close()
	m.host = nil
	return err
}

// HostUDPRelay is shared by video/ICE and Relay's already encrypted Opus UDP.
// It never decodes application payloads or forwards to a non-loopback service.
type HostUDPRelay struct {
	host        net.PacketConn
	localPort   uint16
	maxDatagram int
	mu          sync.Mutex
	closed      bool
	bridges     map[netip.AddrPort]*mediaUDPBridge
	done        chan struct{}
	wg          sync.WaitGroup
	closeOnce   sync.Once
	closeErr    error
}

func NewHostUDPRelay(provider NetPacketProvider, localPort uint16, maxDatagram int) (*HostUDPRelay, error) {
	if provider == nil || localPort == 0 || maxDatagram < 1 || maxDatagram > 65507 {
		return nil, errors.New("a private UDP service and bounded datagram size are required")
	}
	ownIP, _ := provider.TailscaleIPs()
	if !ownIP.Is4() || !meshIPv4Range.Contains(ownIP) {
		return nil, errors.New("private network is not ready for hosting UDP")
	}
	// LiveKit's NAT mapping rewrites candidate IPs but retains its UDP port.
	host, err := provider.ListenPacket("udp4", net.JoinHostPort(ownIP.String(), strconv.Itoa(int(localPort))))
	if err != nil {
		return nil, err
	}
	relay := &HostUDPRelay{
		host: host, localPort: localPort, maxDatagram: maxDatagram,
		bridges: make(map[netip.AddrPort]*mediaUDPBridge), done: make(chan struct{}),
	}
	relay.wg.Add(2)
	go relay.readHost()
	go relay.expireBridges()
	return relay, nil
}

func (m *HostUDPRelay) readHost() {
	defer m.wg.Done()
	payload := make([]byte, m.maxDatagram+1)
	for {
		n, source, err := m.host.ReadFrom(payload)
		if err != nil {
			return
		}
		peer, ok := source.(*net.UDPAddr)
		if !ok || n > m.maxDatagram || peer.Port <= 0 || peer.Port > 65535 {
			continue
		}
		ip, valid := netip.AddrFromSlice(peer.IP)
		ip = ip.Unmap()
		if !valid || !ip.Is4() || !meshIPv4Range.Contains(ip) {
			continue
		}
		tuple := netip.AddrPortFrom(ip, uint16(peer.Port))
		bridge := m.getBridge(tuple)
		if bridge != nil {
			_, _ = bridge.conn.Write(payload[:n])
		}
	}
}

func (m *HostUDPRelay) getBridge(tuple netip.AddrPort) *mediaUDPBridge {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return nil
	}
	if bridge := m.bridges[tuple]; bridge != nil {
		bridge.last = time.Now()
		return bridge
	}
	if len(m.bridges) >= maxMediaBridges {
		return nil
	}
	conn, err := net.DialUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)}, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: int(m.localPort)})
	if err != nil {
		return nil
	}
	bridge := &mediaUDPBridge{conn: conn, last: time.Now()}
	m.bridges[tuple] = bridge
	m.wg.Add(1)
	go m.readService(tuple, bridge)
	return bridge
}

func (m *HostUDPRelay) readService(tuple netip.AddrPort, bridge *mediaUDPBridge) {
	defer m.wg.Done()
	defer func() {
		_ = bridge.conn.Close()
		m.mu.Lock()
		if m.bridges[tuple] == bridge {
			delete(m.bridges, tuple)
		}
		m.mu.Unlock()
	}()
	payload := make([]byte, m.maxDatagram+1)
	for {
		n, err := bridge.conn.Read(payload)
		if err != nil {
			return
		}
		if n > m.maxDatagram {
			continue
		}
		m.mu.Lock()
		bridge.last = time.Now()
		m.mu.Unlock()
		_, _ = m.host.WriteTo(payload[:n], net.UDPAddrFromAddrPort(tuple))
	}
}

func (m *HostUDPRelay) expireBridges() {
	defer m.wg.Done()
	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-m.done:
			return
		case now := <-ticker.C:
			m.mu.Lock()
			for tuple, bridge := range m.bridges {
				if now.Sub(bridge.last) > mediaBridgeIdle {
					delete(m.bridges, tuple)
					_ = bridge.conn.Close()
				}
			}
			m.mu.Unlock()
		}
	}
}

func (m *HostUDPRelay) Close() error {
	m.closeOnce.Do(func() {
		m.mu.Lock()
		m.closed = true
		close(m.done)
		host := m.host
		bridges := make([]*mediaUDPBridge, 0, len(m.bridges))
		for _, bridge := range m.bridges {
			bridges = append(bridges, bridge)
		}
		m.mu.Unlock()
		if host != nil {
			m.closeErr = host.Close()
		}
		for _, bridge := range bridges {
			m.closeErr = errors.Join(m.closeErr, bridge.conn.Close())
		}
		m.wg.Wait()
	})
	return m.closeErr
}

func (m *MediaRelay) Close() error {
	m.closeOnce.Do(func() {
		m.mu.Lock()
		m.closed = true
		host := m.host
		m.mu.Unlock()
		if host != nil {
			m.closeErr = host.Close()
		}
		m.closeErr = errors.Join(m.closeErr, m.turn.Close())
	})
	return m.closeErr
}
