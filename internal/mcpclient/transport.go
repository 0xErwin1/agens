package mcpclient

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"os/exec"

	"github.com/0xErwin1/agens/internal/config"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type SDKConnector struct{}

func (SDKConnector) Connect(ctx context.Context, server config.MCPServer) (Session, error) {
	transport, err := NewTransport(server)
	if err != nil {
		return nil, err
	}
	client := mcp.NewClient(&mcp.Implementation{Name: "agens", Version: "dev"}, nil)
	session, err := client.Connect(ctx, transport, nil)
	if err != nil {
		return nil, err
	}
	return sdkSession{session: session}, nil
}

type sdkSession struct {
	session *mcp.ClientSession
}

func (s sdkSession) ListTools(ctx context.Context) ([]*mcp.Tool, error) {
	result, err := s.session.ListTools(ctx, nil)
	if err != nil {
		return nil, err
	}
	return result.Tools, nil
}

func (s sdkSession) CallTool(ctx context.Context, params *mcp.CallToolParams) (*mcp.CallToolResult, error) {
	return s.session.CallTool(ctx, params)
}

func (s sdkSession) Close() error {
	return s.session.Close()
}

func NewTransport(server config.MCPServer) (mcp.Transport, error) {
	switch server.Transport {
	case config.MCPTransportStdio:
		cmd := exec.Command(server.Command, server.Args...)
		cmd.Dir = server.CWD
		cmd.Env = os.Environ()
		for key, value := range server.Env {
			cmd.Env = append(cmd.Env, key+"="+value)
		}
		return &mcp.CommandTransport{Command: cmd}, nil
	case config.MCPTransportHTTP:
		return &mcp.StreamableClientTransport{
			Endpoint:   server.URL,
			HTTPClient: httpClientWithHeaders(server.Headers, http.DefaultTransport, server.URL),
			MaxRetries: server.MaxRetries,
		}, nil
	case config.MCPTransportSSE:
		return &mcp.SSEClientTransport{
			Endpoint:   server.URL,
			HTTPClient: httpClientWithHeaders(server.Headers, http.DefaultTransport, server.URL),
		}, nil
	default:
		return nil, fmt.Errorf("unsupported MCP transport %q", server.Transport)
	}
}

var ErrCrossOriginRedirect = errors.New("mcp client refused cross-origin redirect with configured headers")

func httpClientWithHeaders(headers map[string]string, base http.RoundTripper, endpoint string) *http.Client {
	if base == nil {
		base = http.DefaultTransport
	}
	origin := endpointOrigin(endpoint)
	return &http.Client{
		Transport: headerRoundTripper{headers: headers, base: base, origin: origin},
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			if len(headers) == 0 || sameOrigin(req.URL, origin) {
				return nil
			}
			return ErrCrossOriginRedirect
		},
	}
}

type headerRoundTripper struct {
	headers map[string]string
	base    http.RoundTripper
	origin  urlOrigin
}

func (h headerRoundTripper) RoundTrip(req *http.Request) (*http.Response, error) {
	cloned := req.Clone(req.Context())
	if sameOrigin(cloned.URL, h.origin) {
		for key, value := range h.headers {
			cloned.Header.Set(key, value)
		}
	}
	return h.base.RoundTrip(cloned)
}

type urlOrigin struct {
	scheme string
	host   string
}

func endpointOrigin(endpoint string) urlOrigin {
	parsed, err := url.Parse(endpoint)
	if err != nil {
		return urlOrigin{}
	}
	return urlOrigin{scheme: parsed.Scheme, host: parsed.Host}
}

func sameOrigin(u *url.URL, origin urlOrigin) bool {
	return u != nil && u.Scheme == origin.scheme && u.Host == origin.host
}
