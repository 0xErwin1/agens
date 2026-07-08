package mcpclient

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"os/exec"
	"testing"

	"github.com/0xErwin1/agens/internal/config"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func TestNewTransportCreatesSDKStdioShapeWithoutStartingCommand(t *testing.T) {
	transport, err := NewTransport(config.MCPServer{
		Transport: config.MCPTransportStdio,
		Command:   "printf",
		Args:      []string{"hello"},
		Env:       map[string]string{"TOKEN": "secret"},
		CWD:       t.TempDir(),
	})
	if err != nil {
		t.Fatalf("NewTransport error = %v", err)
	}
	stdio, ok := transport.(*mcp.CommandTransport)
	if !ok {
		t.Fatalf("transport type = %T, want *mcp.CommandTransport", transport)
	}
	if stdio.Command == nil {
		t.Fatal("CommandTransport.Command = nil")
	}
	if stdio.Command.Args[0] != "printf" {
		t.Fatalf("Command.Args[0] = %q, want printf", stdio.Command.Args[0])
	}
	if len(stdio.Command.Args) != 2 || stdio.Command.Args[1] != "hello" {
		t.Fatalf("Command.Args = %#v, want command plus hello", stdio.Command.Args)
	}
	if stdio.Command.Process != nil {
		t.Fatal("stdio command was started during transport construction")
	}
}

func TestNewTransportCreatesSDKStreamableHTTPShape(t *testing.T) {
	transport, err := NewTransport(config.MCPServer{Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp", Headers: map[string]string{"Authorization": "Bearer token"}, MaxRetries: 2})
	if err != nil {
		t.Fatalf("NewTransport error = %v", err)
	}
	httpTransport, ok := transport.(*mcp.StreamableClientTransport)
	if !ok {
		t.Fatalf("transport type = %T, want *mcp.StreamableClientTransport", transport)
	}
	if httpTransport.Endpoint != "https://example.test/mcp" || httpTransport.MaxRetries != 2 {
		t.Fatalf("http transport = %#v, want endpoint and retries", httpTransport)
	}
	if httpTransport.HTTPClient == nil {
		t.Fatal("HTTPClient = nil, want header-aware client")
	}
}

func TestNewTransportCreatesSDKSSEShape(t *testing.T) {
	transport, err := NewTransport(config.MCPServer{Transport: config.MCPTransportSSE, URL: "https://example.test/sse", Headers: map[string]string{"Authorization": "Bearer token"}})
	if err != nil {
		t.Fatalf("NewTransport error = %v", err)
	}
	sseTransport, ok := transport.(*mcp.SSEClientTransport)
	if !ok {
		t.Fatalf("transport type = %T, want *mcp.SSEClientTransport", transport)
	}
	if sseTransport.Endpoint != "https://example.test/sse" {
		t.Fatalf("Endpoint = %q, want SSE URL", sseTransport.Endpoint)
	}
	if sseTransport.HTTPClient == nil {
		t.Fatal("HTTPClient = nil, want header-aware client")
	}
}

func TestAllConfigTransportsAcceptedThroughDiscoveryPath(t *testing.T) {
	connector := &fakeConnector{sessions: []*fakeSession{{}, {}, {}}}
	client := New(map[string]config.MCPServer{
		"files": {Transport: config.MCPTransportStdio, Command: "printf"},
		"docs":  {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"},
		"sse":   {Transport: config.MCPTransportSSE, URL: "https://example.test/sse"},
	}, WithConnector(connector))

	tools, diagnostics := client.Discover(context.Background())
	if len(tools) != 0 || len(diagnostics) != 0 {
		t.Fatalf("Discover tools/diagnostics = %d/%#v, want empty success", len(tools), diagnostics)
	}
	if len(connector.seen) != 3 {
		t.Fatalf("connector saw %d transports, want 3", len(connector.seen))
	}
	seen := map[string]bool{}
	for _, server := range connector.seen {
		seen[server.Transport] = true
	}
	for _, transport := range []string{config.MCPTransportStdio, config.MCPTransportHTTP, config.MCPTransportSSE} {
		if !seen[transport] {
			t.Fatalf("transport %q not seen in discovery path", transport)
		}
	}
}

func TestHTTPClientWithHeadersRejectsCrossOriginRedirects(t *testing.T) {
	redirectTarget := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "" {
			t.Fatalf("redirect target received Authorization = %q, want none", got)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	t.Cleanup(redirectTarget.Close)

	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, redirectTarget.URL, http.StatusFound)
	}))
	t.Cleanup(origin.Close)

	client := httpClientWithHeaders(map[string]string{"Authorization": "Bearer token"}, http.DefaultTransport, origin.URL)
	_, err := client.Get(origin.URL)
	if err == nil {
		t.Fatal("client.Get error = nil, want cross-origin redirect refusal")
	}
	if !errors.Is(err, ErrCrossOriginRedirect) {
		t.Fatalf("client.Get error = %v, want ErrCrossOriginRedirect", err)
	}
}

func TestHeaderRoundTripperAddsConfiguredHeaders(t *testing.T) {
	base := roundTripFunc(func(req *http.Request) (*http.Response, error) {
		if req.Header.Get("Authorization") != "Bearer token" {
			t.Fatalf("Authorization = %q, want configured header", req.Header.Get("Authorization"))
		}
		return &http.Response{StatusCode: http.StatusNoContent, Body: http.NoBody, Header: http.Header{}}, nil
	})
	client := httpClientWithHeaders(map[string]string{"Authorization": "Bearer token"}, base, "https://example.test/mcp")
	_, err := client.Get("https://example.test")
	if err != nil {
		t.Fatalf("client.Get error = %v", err)
	}
}

func TestSDKStdioUsesExecCommandShape(t *testing.T) {
	transport, err := NewTransport(config.MCPServer{Transport: config.MCPTransportStdio, Command: "echo"})
	if err != nil {
		t.Fatalf("NewTransport error = %v", err)
	}
	stdio := transport.(*mcp.CommandTransport)
	if reflectType := reflectCommandType(stdio.Command); reflectType != "*exec.Cmd" {
		t.Fatalf("Command type = %q, want *exec.Cmd", reflectType)
	}
}

func reflectCommandType(cmd *exec.Cmd) string {
	if cmd == nil {
		return "<nil>"
	}
	return "*exec.Cmd"
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return f(req)
}
