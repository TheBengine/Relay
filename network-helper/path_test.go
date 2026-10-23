package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/netip"
	"strings"
	"testing"
	"time"

	"tailscale.com/ipn/ipnstate"
)

func TestClassifyPingReportsHowThePeerIsReachedWithoutItsAddress(t *testing.T) {
	cases := []struct {
		name   string
		result *ipnstate.PingResult
		want   pathInfo
	}{
		{"direct", &ipnstate.PingResult{Endpoint: "203.0.113.9:41641", LatencySeconds: 0.012}, pathInfo{Kind: pathDirect, RTTMillis: 12}},
		{"peer relay", &ipnstate.PingResult{PeerRelay: "198.51.100.3:3478:vni:7", LatencySeconds: 0.02}, pathInfo{Kind: pathPeerRelay, RTTMillis: 20}},
		{"derp", &ipnstate.PingResult{DERPRegionID: 10, DERPRegionCode: "fra", LatencySeconds: 0.041}, pathInfo{Kind: pathDERP, Region: "fra", RTTMillis: 41}},
		{"derp with hostile region text", &ipnstate.PingResult{DERPRegionID: 3, DERPRegionCode: "FRA <b>", LatencySeconds: 0.03}, pathInfo{Kind: pathDERP, RTTMillis: 30}},
		{"no path information", &ipnstate.PingResult{LatencySeconds: 0.005}, pathInfo{Kind: pathUnknown, RTTMillis: 5}},
		{"implausible latency is dropped", &ipnstate.PingResult{Endpoint: "203.0.113.9:1", LatencySeconds: -4}, pathInfo{Kind: pathDirect}},
		{"endpoint wins over relay fields", &ipnstate.PingResult{Endpoint: "203.0.113.9:1", DERPRegionID: 1, DERPRegionCode: "fra", LatencySeconds: 0.001}, pathInfo{Kind: pathDirect, RTTMillis: 1}},
	}
	for _, c := range cases {
		got, err := classifyPing(c.result)
		if err != nil {
			t.Fatalf("%s: %v", c.name, err)
		}
		if got.Kind != c.want.Kind || got.Region != c.want.Region || got.RTTMillis < c.want.RTTMillis-0.001 || got.RTTMillis > c.want.RTTMillis+0.001 {
			t.Fatalf("%s: got %+v want %+v", c.name, got, c.want)
		}
		encoded, _ := json.Marshal(got)
		if strings.Contains(string(encoded), "203.0.113") || strings.Contains(string(encoded), "198.51.100") {
			t.Fatalf("%s: a peer address leaked into %s", c.name, encoded)
		}
	}
	if _, err := classifyPing(nil); err == nil {
		t.Fatal("nil result accepted")
	}
	if got, err := classifyPing(&ipnstate.PingResult{Err: "timeout"}); err == nil || got.Kind != pathUnknown {
		t.Fatal("failed ping was classified", got)
	}
}

type pathFakeNetwork struct {
	*fakeRelayNetwork
	result *ipnstate.PingResult
	err    error
	pinged []netip.Addr
}

func (n *pathFakeNetwork) ProbePath(_ context.Context, peer netip.Addr) (*ipnstate.PingResult, error) {
	n.mu.Lock()
	n.pinged = append(n.pinged, peer)
	n.mu.Unlock()
	return n.result, n.err
}

func readyPathHelper(t *testing.T, n *pathFakeNetwork) *helper {
	t.Helper()
	h := newHelper(strings.Repeat("a", 64), func(func(string)) relayNetwork { return n })
	t.Cleanup(h.close)
	if w := controlCall(t, h, "POST", "/connect", "{}"); w.Code != 200 {
		t.Fatal(w.Code)
	}
	deadline := time.Now().Add(2 * time.Second)
	for {
		h.mu.Lock()
		phase := h.status.Phase
		h.mu.Unlock()
		if phase == "ready" {
			return h
		}
		if time.Now().After(deadline) {
			t.Fatal("fake network did not become ready")
		}
		time.Sleep(time.Millisecond)
	}
}

func TestPathEndpointMeasuresOnlyRoutesTheAppAlreadyOpened(t *testing.T) {
	n := &pathFakeNetwork{
		fakeRelayNetwork: newFakeRelayNetwork(),
		result:           &ipnstate.PingResult{DERPRegionID: 10, DERPRegionCode: "fra", LatencySeconds: 0.033},
	}
	h := readyPathHelper(t, n)

	// No route yet: nothing may be probed, so the endpoint cannot be used to scan.
	if w := controlCall(t, h, "POST", "/path", `{"host":"100.100.5.7"}`); w.Code != 409 {
		t.Fatal("an unrouted peer was measured", w.Code, w.Body.String())
	}
	if len(n.pinged) != 0 {
		t.Fatal("ping sent for an unknown peer", n.pinged)
	}
	if w := controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580}`); w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	w := controlCall(t, h, "POST", "/path", `{"host":"100.100.5.7"}`)
	if w.Code != 200 {
		t.Fatal(w.Code, w.Body.String())
	}
	var got pathInfo
	if json.Unmarshal(w.Body.Bytes(), &got) != nil || got.Kind != pathDERP || got.Region != "fra" || got.RTTMillis < 32 || got.RTTMillis > 34 {
		t.Fatal(w.Body.String())
	}
	if len(n.pinged) != 1 || n.pinged[0].String() != "100.100.5.7" {
		t.Fatal(n.pinged)
	}
	for _, body := range []string{`{"host":"8.8.8.8"}`, `{"host":"100.100.5.7","extra":1}`, `{"host":"not-an-ip"}`, `{}`} {
		if w := controlCall(t, h, "POST", "/path", body); w.Code != 400 {
			t.Fatal("invalid path request accepted", body, w.Code)
		}
	}
}

func TestPathEndpointTurnsMeasurementFailureIntoUnknownNotAnError(t *testing.T) {
	n := &pathFakeNetwork{fakeRelayNetwork: newFakeRelayNetwork(), err: errors.New("peer is offline")}
	h := readyPathHelper(t, n)
	if w := controlCall(t, h, "POST", "/route", `{"host":"100.100.5.7","port":4580}`); w.Code != 200 {
		t.Fatal(w.Code)
	}
	w := controlCall(t, h, "POST", "/path", `{"host":"100.100.5.7"}`)
	var got pathInfo
	if w.Code != 200 || json.Unmarshal(w.Body.Bytes(), &got) != nil || got.Kind != pathUnknown || got.RTTMillis != 0 {
		t.Fatal(w.Code, w.Body.String())
	}
}

func TestProbePathWithoutMeasurementSupportReportsUnknown(t *testing.T) {
	// A network which cannot measure (the plain fake) never invents a path.
	if info, err := probePath(context.Background(), newFakeRelayNetwork(), netip.MustParseAddr("100.100.5.7")); err == nil || info.Kind != pathUnknown {
		t.Fatal("a network without measurement support reported a path", info)
	}
}
