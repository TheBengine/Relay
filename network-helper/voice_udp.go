package main

import (
	"context"
	"errors"
	"net"
	"net/netip"
	"strconv"
	"sync"
	"time"
)

const maxVoiceDatagram = 2048

type privateUDPDialer interface {
	Dial(context.Context, string, string) (net.Conn, error)
}

// One bridge belongs to one native voice socket/session. Bytes remain opaque;
// the existing Relay voice protocol authenticates and encrypts their contents.
type ClientVoiceRelay struct {
	local  *net.UDPConn
	remote net.Conn
	mu     sync.Mutex
	source netip.AddrPort
	wg     sync.WaitGroup
	once   sync.Once
}

func NewClientVoiceRelay(provider privateUDPDialer, host netip.Addr, port uint16) (*ClientVoiceRelay, error) {
	if provider == nil || !host.Is4() || !meshIPv4Range.Contains(host) || port == 0 {
		return nil, errors.New("private voice route requires a selected Relay host and port")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	remote, err := provider.Dial(ctx, "udp", net.JoinHostPort(host.String(), strconv.Itoa(int(port))))
	if err != nil {
		return nil, err
	}
	local, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		_ = remote.Close()
		return nil, err
	}
	relay := &ClientVoiceRelay{local: local, remote: remote}
	relay.wg.Add(2)
	go relay.readLocal()
	go relay.readRemote()
	return relay, nil
}
func (v *ClientVoiceRelay) Port() uint16 { return uint16(v.local.LocalAddr().(*net.UDPAddr).Port) }
func (v *ClientVoiceRelay) readLocal() {
	defer v.wg.Done()
	buffer := make([]byte, maxVoiceDatagram+1)
	for {
		n, source, err := v.local.ReadFromUDPAddrPort(buffer)
		if err != nil {
			return
		}
		if n == 0 || n > maxVoiceDatagram || !source.Addr().IsLoopback() {
			continue
		}
		v.mu.Lock()
		if !v.source.IsValid() {
			v.source = source
		}
		allowed := v.source == source
		v.mu.Unlock()
		if allowed {
			_, _ = v.remote.Write(buffer[:n])
		}
	}
}
func (v *ClientVoiceRelay) readRemote() {
	defer v.wg.Done()
	buffer := make([]byte, maxVoiceDatagram+1)
	for {
		n, err := v.remote.Read(buffer)
		if err != nil {
			return
		}
		if n == 0 || n > maxVoiceDatagram {
			continue
		}
		v.mu.Lock()
		source := v.source
		v.mu.Unlock()
		if source.IsValid() {
			_, _ = v.local.WriteToUDPAddrPort(buffer[:n], source)
		}
	}
}
func (v *ClientVoiceRelay) Close() error {
	var result error
	v.once.Do(func() { result = errors.Join(v.local.Close(), v.remote.Close()); v.wg.Wait() })
	return result
}
