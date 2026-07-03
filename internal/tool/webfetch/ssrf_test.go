package webfetch

import (
	"errors"
	"net/http"
	"testing"
)

func TestSsrfControl(t *testing.T) {
	tests := []struct {
		name    string
		address string
		block   bool
	}{
		{name: "metadata IPv4", address: "169.254.169.254:80", block: true},
		{name: "link-local IPv6", address: "[fe80::1]:80", block: true},
		{name: "4-in-6 mapped metadata", address: "[::ffff:169.254.169.254]:80", block: true},
		{name: "unparsable address", address: "not-an-address", block: true},

		{name: "loopback IPv4", address: "127.0.0.1:80", block: false},
		{name: "loopback IPv6", address: "[::1]:80", block: false},
		{name: "private 10/8", address: "10.0.0.1:80", block: false},
		{name: "private 172.16/12", address: "172.16.0.1:80", block: false},
		{name: "private 192.168/16", address: "192.168.1.1:80", block: false},
		{name: "public IP", address: "8.8.8.8:80", block: false},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			err := ssrfControl("tcp4", tc.address, nil)
			if tc.block {
				if !errors.Is(err, errHostNotAllowed) {
					t.Fatalf("ssrfControl(%q) = %v, want errHostNotAllowed", tc.address, err)
				}
				return
			}
			if err != nil {
				t.Fatalf("ssrfControl(%q) = %v, want nil", tc.address, err)
			}
		})
	}
}

func TestNewClient(t *testing.T) {
	client := newClient()
	if client == nil {
		t.Fatal("newClient() = nil, want non-nil")
	}
	if client.Timeout != 0 {
		t.Fatalf("client.Timeout = %v, want 0 (per-request ctx drives timeout)", client.Timeout)
	}

	transport, ok := client.Transport.(*http.Transport)
	if !ok {
		t.Fatalf("client.Transport = %T, want *http.Transport", client.Transport)
	}
	if defaultTransport, ok := http.DefaultTransport.(*http.Transport); ok && transport == defaultTransport {
		t.Fatal("client.Transport is the shared http.DefaultTransport pointer, want a Clone()")
	}
	if transport.DialContext == nil {
		t.Fatal("transport.DialContext = nil, want a custom dialer with SSRF Control")
	}
}
