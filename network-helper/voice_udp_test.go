package main

import (
	"bytes"
	"context"
	"errors"
	"net"
	"net/netip"
	"testing"
	"time"
)

type fixtureVoiceDialer struct {
	target   *net.UDPAddr
	expected string
}

func (d fixtureVoiceDialer) Dial(_ context.Context, network, address string) (net.Conn, error) {
	if network != "udp" || address != d.expected {
		return nil, errors.New("unexpected UDP target")
	}
	return net.DialUDP("udp4", nil, d.target)
}

func TestClientVoiceOpaqueDatagramsSourceIsolationAndClose(t *testing.T) {
	peer, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	defer peer.Close()
	bridge, err := NewClientVoiceRelay(fixtureVoiceDialer{target: peer.LocalAddr().(*net.UDPAddr), expected: "100.100.5.7:4581"}, netip.MustParseAddr("100.100.5.7"), 4581)
	if err != nil {
		t.Fatal(err)
	}
	defer bridge.Close()
	native, err := net.DialUDP("udp4", nil, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: int(bridge.Port())})
	if err != nil {
		t.Fatal(err)
	}
	defer native.Close()
	encrypted := []byte{0, 255, 7, 13, 20, 60, 4, 9, 99}
	_, _ = native.Write(encrypted)
	_ = peer.SetReadDeadline(time.Now().Add(time.Second))
	buffer := make([]byte, 4096)
	n, from, err := peer.ReadFromUDP(buffer)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(buffer[:n], encrypted) {
		t.Fatal("encrypted packet altered")
	}
	_, _ = peer.WriteToUDP(encrypted, from)
	_ = native.SetReadDeadline(time.Now().Add(time.Second))
	n, err = native.Read(buffer)
	if err != nil || !bytes.Equal(buffer[:n], encrypted) {
		t.Fatal("opaque reply altered", err)
	}
	other, err := net.DialUDP("udp4", nil, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: int(bridge.Port())})
	if err != nil {
		t.Fatal(err)
	}
	defer other.Close()
	_, _ = other.Write([]byte("different local process"))
	_, _ = native.Write(make([]byte, maxVoiceDatagram+1))
	_ = peer.SetReadDeadline(time.Now().Add(80 * time.Millisecond))
	if _, _, err = peer.ReadFromUDP(buffer); err == nil {
		t.Fatal("different source or oversize packet forwarded")
	}
	_, _ = native.Write([]byte("same socket continues"))
	_ = peer.SetReadDeadline(time.Now().Add(time.Second))
	n, _, err = peer.ReadFromUDP(buffer)
	if err != nil || string(buffer[:n]) != "same socket continues" {
		t.Fatal("original source stopped", err)
	}
	port := bridge.Port()
	_ = bridge.Close()
	rebound, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: int(port)})
	if err != nil {
		t.Fatal("voice listener survived cleanup", err)
	}
	_ = rebound.Close()
}

func TestClientVoiceRequiresSelectedPrivateDestination(t *testing.T) {
	for _, host := range []string{"8.8.8.8", "127.0.0.1", "192.168.1.1"} {
		if _, err := NewClientVoiceRelay(fixtureVoiceDialer{}, netip.MustParseAddr(host), 4581); err == nil {
			t.Fatal("nonprivate destination accepted", host)
		}
	}
	if _, err := NewClientVoiceRelay(fixtureVoiceDialer{}, netip.MustParseAddr("100.100.5.7"), 0); err == nil {
		t.Fatal("zero UDP service allowed")
	}
}
