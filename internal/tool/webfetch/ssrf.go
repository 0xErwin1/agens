package webfetch

import (
	"errors"
	"net"
	"net/http"
	"syscall"
	"time"
)

// errHostNotAllowed is returned when ssrfControl rejects a dial attempt. The
// message deliberately omits the resolved IP: it is surfaced verbatim to the
// model via tool.Result.Text, and leaking an internal address there would
// defeat the guard it is meant to enforce.
var errHostNotAllowed = errors.New("webfetch: host not allowed")

// ssrfControl is installed as net.Dialer.Control on the client built by
// newClient. It runs once per connection attempt, after DNS resolution but
// before the socket connects, on the actual resolved ip:port — not the
// original hostname. That timing matters for two reasons a hostname-based
// check cannot cover: an HTTP redirect can point at a different host on each
// hop, and DNS rebinding can make the same hostname resolve to a different
// (attacker-controlled) address between the initial check and the real
// connect. Running the check at dial time re-validates every hop and every
// resolution, closing both gaps.
//
// Loopback (127.0.0.0/8, ::1) and private ranges (10/8, 172.16/12,
// 192.168/16, fc00::/7) are intentionally allowed: fetching a locally running
// dev server is a legitimate use case, and this guard's job is narrowly to
// block link-local and cloud-metadata addresses (169.254.0.0/16, fe80::/10),
// not to sandbox the network generally.
//
// Any failure to parse the dial address is treated as a block: fail closed,
// never open, when the guard cannot classify what it is looking at.
func ssrfControl(_, address string, _ syscall.RawConn) error {
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return errHostNotAllowed
	}

	ip := net.ParseIP(host)
	if ip == nil {
		return errHostNotAllowed
	}

	if ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() {
		return errHostNotAllowed
	}

	return nil
}

// newClient builds the *http.Client used by the webfetch tool. It clones
// http.DefaultTransport (keeping its proxy, TLS, and HTTP/2 defaults) and
// replaces DialContext with a dialer that runs ssrfControl on every
// connection attempt, including redirect hops. Client.Timeout is left at its
// zero value: the caller derives a per-request deadline from the turn
// context instead, so a single shared client can serve calls with different
// timeouts.
func newClient() *http.Client {
	transport := http.DefaultTransport.(*http.Transport).Clone()

	dialer := &net.Dialer{
		Timeout:   30 * time.Second,
		KeepAlive: 30 * time.Second,
		Control:   ssrfControl,
	}
	transport.DialContext = dialer.DialContext

	return &http.Client{Transport: transport}
}
