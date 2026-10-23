package main

// Native WebRTC talks to authenticated localhost TURN. Its UDP allocations use
// independent, admitted iroh streams to the host's fixed SFU, without a VPN node.
import (
	"bufio"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"strconv"
	"sync"
	"time"
)

type iroMediaProvider struct {
	own     netip.Addr
	gateway string
	host    netip.Addr
	port    uint16
}

func newIrohMediaProvider(gateway, host string, port uint16) (*iroMediaProvider, error) {
	ip, err := netip.ParseAddr(host)
	if err != nil || ip.String() != "100.127.255.254" || port == 0 {
		return nil, errors.New("fixed account-free video destination required")
	}
	if gateway != "" {
		u, err := url.Parse(gateway)
		if err != nil || u.Scheme != "http" || u.Hostname() != "127.0.0.1" ||
			u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.Opaque != "" ||
			u.Path != "/" && u.Path != "" {
			return nil, errors.New("owned loopback video gateway required")
		}
		p, err := strconv.ParseUint(u.Port(), 10, 16)
		if err != nil || p == 0 {
			return nil, errors.New("video gateway port required")
		}
		gateway = u.Host
	}
	random, err := randomMediaCredential()
	if err != nil {
		return nil, err
	}
	hash := sha256.Sum256([]byte(random))
	own := netip.AddrFrom4([4]byte{100, 64 + hash[0]%62, hash[1], hash[2]})
	return &iroMediaProvider{gateway: gateway, host: ip, port: port, own: own}, nil
}

func (p *iroMediaProvider) TailscaleIPs() (netip.Addr, netip.Addr) {
	// A TURN allocation address, never assigned to an OS interface.
	return p.own, netip.Addr{}
}

func (p *iroMediaProvider) ListenPacket(network, address string) (net.PacketConn, error) {
	if network != "udp4" || address != net.JoinHostPort(p.own.String(), "0") {
		return nil, errors.New("only an ephemeral video allocation is allowed")
	}
	packet := &iroMediaPacket{peer: &net.UDPAddr{IP: net.IP(p.host.AsSlice()), Port: int(p.port)}}
	if p.gateway == "" {
		conn, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
		if err != nil {
			return nil, err
		}
		packet.local = conn
		packet.address = &net.UDPAddr{IP: net.IP(p.own.AsSlice()), Port: conn.LocalAddr().(*net.UDPAddr).Port}
		return packet, nil
	}
	conn, err := net.DialTimeout("tcp4", p.gateway, 5*time.Second)
	if err != nil {
		return nil, errors.New("account-free video gateway unavailable")
	}
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
	request, _ := http.NewRequest(http.MethodPost, "http://"+p.gateway+"/_relay/media", http.NoBody)
	request.Header.Set("Connection", "upgrade")
	request.Header.Set("Upgrade", "relay-media-v1")
	request.Header.Set("Content-Length", "0")
	if err = request.Write(conn); err != nil {
		_ = conn.Close()
		return nil, err
	}
	reader := bufio.NewReaderSize(conn, 4096)
	response, err := http.ReadResponse(reader, request)
	if err != nil || response.StatusCode != http.StatusSwitchingProtocols || response.Header.Get("Upgrade") != "relay-media-v1" {
		_ = conn.Close()
		return nil, errors.New("host needs the latest Relay for account-free video")
	}
	_ = conn.SetDeadline(time.Time{})
	packet.stream, packet.reader = conn, reader
	packet.address = &net.UDPAddr{IP: net.IP(p.own.AsSlice()), Port: conn.LocalAddr().(*net.TCPAddr).Port}
	return packet, nil
}

type iroMediaPacket struct {
	peer    *net.UDPAddr
	address *net.UDPAddr
	stream  net.Conn
	reader  *bufio.Reader
	local   *net.UDPConn
	writeMu sync.Mutex
	readMu  sync.Mutex
}

func (p *iroMediaPacket) LocalAddr() net.Addr { return p.address }
func (p *iroMediaPacket) Close() error {
	if p.local != nil {
		return p.local.Close()
	}
	return p.stream.Close()
}
func (p *iroMediaPacket) WriteTo(payload []byte, destination net.Addr) (int, error) {
	target, ok := destination.(*net.UDPAddr)
	if !ok || !target.IP.Equal(p.peer.IP) || target.Port != p.peer.Port || len(payload) == 0 || len(payload) > maxMediaDatagram {
		return 0, errors.New("video destination or packet is not permitted")
	}
	if p.local != nil {
		return p.local.WriteToUDP(payload, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: p.peer.Port})
	}
	p.writeMu.Lock()
	defer p.writeMu.Unlock()
	frame := make([]byte, 2+len(payload))
	binary.BigEndian.PutUint16(frame, uint16(len(payload)))
	copy(frame[2:], payload)
	for len(frame) > 0 {
		n, err := p.stream.Write(frame)
		if err != nil {
			return 0, err
		}
		if n == 0 {
			return 0, io.ErrShortWrite
		}
		frame = frame[n:]
	}
	return len(payload), nil
}
func (p *iroMediaPacket) ReadFrom(payload []byte) (int, net.Addr, error) {
	p.readMu.Lock()
	defer p.readMu.Unlock()
	if p.local != nil {
		var buffer [maxMediaDatagram + 1]byte
		for {
			n, from, err := p.local.ReadFromUDP(buffer[:])
			if err != nil {
				return 0, nil, err
			}
			if from.IP.Equal(net.IPv4(127, 0, 0, 1)) && from.Port == p.peer.Port && n <= maxMediaDatagram {
				if n > len(payload) {
					return 0, nil, io.ErrShortBuffer
				}
				copy(payload, buffer[:n])
				return n, p.peer, nil
			}
		}
	}
	var header [2]byte
	if _, err := io.ReadFull(p.reader, header[:]); err != nil {
		return 0, nil, err
	}
	n := int(binary.BigEndian.Uint16(header[:]))
	if n == 0 || n > maxMediaDatagram {
		_ = p.stream.Close()
		return 0, nil, errors.New("invalid video packet")
	}
	if n > len(payload) {
		_ = p.stream.Close()
		return 0, nil, io.ErrShortBuffer
	}
	if _, err := io.ReadFull(p.reader, payload[:n]); err != nil {
		return 0, nil, err
	}
	return n, p.peer, nil
}
func (p *iroMediaPacket) SetDeadline(t time.Time) error {
	if p.local != nil {
		return p.local.SetDeadline(t)
	}
	return p.stream.SetDeadline(t)
}
func (p *iroMediaPacket) SetReadDeadline(t time.Time) error {
	if p.local != nil {
		return p.local.SetReadDeadline(t)
	}
	return p.stream.SetReadDeadline(t)
}
func (p *iroMediaPacket) SetWriteDeadline(t time.Time) error {
	if p.local != nil {
		return p.local.SetWriteDeadline(t)
	}
	return p.stream.SetWriteDeadline(t)
}

func (h *helper) openIrohMedia(w http.ResponseWriter, r *http.Request) {
	var input struct {
		Gateway string `json:"gateway"`
		Host    string `json:"host"`
		Port    uint16 `json:"port"`
	}
	if decodeControl(w, r, &input) != nil {
		controlError(w, 400, "Invalid video connection")
		return
	}
	provider, err := newIrohMediaProvider(input.Gateway, input.Host, input.Port)
	if err != nil {
		controlError(w, 400, "Invalid fixed video gateway")
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed {
		controlError(w, 409, "Connection helper stopped")
		return
	}
	key := input.Gateway + "/" + strconv.Itoa(int(input.Port))
	if h.irohMedia == nil {
		h.irohMedia = make(map[string]*LocalTurn)
	}
	if old := h.irohMedia[key]; old != nil {
		jsonResponse(w, 200, old.Config)
		return
	}
	if len(h.irohMedia) >= 8 {
		controlError(w, 429, "Video route limit reached")
		return
	}
	turn, err := newLocalTurn(provider, func(ip net.IP) bool { return ip.Equal(net.IP(provider.host.AsSlice())) }, func(addr *net.UDPAddr) bool {
		return addr.IP.Equal(net.IP(provider.host.AsSlice())) && addr.Port == int(provider.port)
	})
	if err != nil {
		controlError(w, 502, "Cannot start account-free video")
		return
	}
	turn.Config.Address = provider.own.String()
	h.irohMedia[key] = turn
	jsonResponse(w, 200, turn.Config)
}
