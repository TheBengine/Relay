package main

import (
	"context"
	"errors"
	"math"
	"net/netip"
	"regexp"
	"time"

	"tailscale.com/ipn/ipnstate"
	"tailscale.com/tailcfg"
)

// Path kinds reported to the app. The peer's public address is deliberately never
// part of this result: the app only needs to know how the connection is carried.
const (
	pathDirect    = "direct"
	pathPeerRelay = "peer_relay"
	pathDERP      = "derp"
	pathUnknown   = "unknown"
)

type pathInfo struct {
	Kind      string  `json:"kind"`
	Region    string  `json:"region,omitempty"`
	RTTMillis float64 `json:"rtt_ms"`
}

// pathProber is implemented by networks which can measure how a peer is reached.
type pathProber interface {
	ProbePath(ctx context.Context, peer netip.Addr) (*ipnstate.PingResult, error)
}

var regionCodePattern = regexp.MustCompile(`\A[a-z0-9-]{1,16}\z`)

// classifyPing converts a Tailscale discovery ping into the small, privacy-preserving
// result the app displays. A failed or empty ping is reported as unknown.
func classifyPing(result *ipnstate.PingResult) (pathInfo, error) {
	if result == nil {
		return pathInfo{Kind: pathUnknown}, errors.New("no ping result")
	}
	if result.Err != "" {
		return pathInfo{Kind: pathUnknown}, errors.New("peer did not answer")
	}
	rtt := result.LatencySeconds * 1000
	if math.IsNaN(rtt) || rtt < 0 || rtt > 60_000 {
		rtt = 0
	}
	info := pathInfo{Kind: pathUnknown, RTTMillis: rtt}
	switch {
	case result.Endpoint != "":
		info.Kind = pathDirect
	case result.PeerRelay != "":
		info.Kind = pathPeerRelay
	case result.DERPRegionID != 0:
		info.Kind = pathDERP
		if regionCodePattern.MatchString(result.DERPRegionCode) {
			info.Region = result.DERPRegionCode
		}
	}
	return info, nil
}

// probePath pings a known peer once. It is bounded in time and never retried here;
// the app polls at a calm interval and tailscaled upgrades the path in the background.
func probePath(ctx context.Context, network any, peer netip.Addr) (pathInfo, error) {
	prober, ok := network.(pathProber)
	if !ok {
		return pathInfo{Kind: pathUnknown}, errors.New("path measurement is unavailable")
	}
	ctx, cancel := context.WithTimeout(ctx, 4*time.Second)
	defer cancel()
	result, err := prober.ProbePath(ctx, peer)
	if err != nil {
		return pathInfo{Kind: pathUnknown}, errors.New("path measurement failed")
	}
	return classifyPing(result)
}

// ProbePath sends one disco ping through the embedded node's local client.
func (n *sdkNetwork) ProbePath(ctx context.Context, peer netip.Addr) (*ipnstate.PingResult, error) {
	client, err := n.server.LocalClient()
	if err != nil {
		return nil, err
	}
	return client.Ping(ctx, peer, tailcfg.PingDisco)
}
