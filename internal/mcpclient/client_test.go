package mcpclient

import (
	"context"
	"encoding/json"
	"errors"
	"reflect"
	"strings"
	"testing"

	"github.com/0xErwin1/agens/internal/config"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type fakeConnector struct {
	sessions []*fakeSession
	err      error
	seen     []config.MCPServer
}

func (f *fakeConnector) Connect(ctx context.Context, server config.MCPServer) (Session, error) {
	f.seen = append(f.seen, server)
	if f.err != nil {
		return nil, f.err
	}
	if len(f.sessions) == 0 {
		return nil, errors.New("no fake session")
	}
	s := f.sessions[0]
	f.sessions = f.sessions[1:]
	return s, nil
}

type fakeSession struct {
	tools       []*mcp.Tool
	listErr     error
	callResult  *mcp.CallToolResult
	callErr     error
	closeErr    error
	closed      bool
	calledName  string
	calledInput any
}

func (f *fakeSession) ListTools(ctx context.Context) ([]*mcp.Tool, error) {
	return f.tools, f.listErr
}

func (f *fakeSession) CallTool(ctx context.Context, params *mcp.CallToolParams) (*mcp.CallToolResult, error) {
	f.calledName = params.Name
	f.calledInput = params.Arguments
	return f.callResult, f.callErr
}

func (f *fakeSession) Close() error {
	f.closed = true
	return f.closeErr
}

func TestDiscoverConvertsSDKToolsToAgensWrappers(t *testing.T) {
	session := &fakeSession{tools: []*mcp.Tool{{
		Name:        "search",
		Description: "Search docs",
		InputSchema: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"query": map[string]any{"type": "string"},
			},
			"required": []any{"query"},
		},
	}}}
	client := New(map[string]config.MCPServer{"docs": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{sessions: []*fakeSession{session}}))

	tools, diagnostics := client.Discover(context.Background())
	if len(diagnostics) != 0 {
		t.Fatalf("Discover diagnostics = %#v, want none", diagnostics)
	}
	if len(tools) != 1 {
		t.Fatalf("Discover tools len = %d, want 1", len(tools))
	}
	if tools[0].Name() != "docs_search" {
		t.Fatalf("tool name = %q, want docs_search", tools[0].Name())
	}
	if tools[0].Description() != "Search docs" {
		t.Fatalf("description = %q, want Search docs", tools[0].Description())
	}
	if tools[0].Schema() == nil {
		t.Fatal("Schema() = nil, want converted schema")
	}
	if !session.closed {
		t.Fatal("discovery session was not closed")
	}
}

func TestDiscoverInvalidSchemaReturnsDiagnostic(t *testing.T) {
	session := &fakeSession{tools: []*mcp.Tool{{
		Name:        "bad",
		Description: "Bad schema",
		InputSchema: map[string]any{"type": 123},
	}}}
	client := New(map[string]config.MCPServer{"docs": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{sessions: []*fakeSession{session}}))

	tools, diagnostics := client.Discover(context.Background())
	if len(tools) != 0 {
		t.Fatalf("Discover tools len = %d, want 0", len(tools))
	}
	if len(diagnostics) != 1 {
		t.Fatalf("Discover diagnostics len = %d, want 1", len(diagnostics))
	}
	if diagnostics[0].Server != "docs" || diagnostics[0].Tool != "bad" || diagnostics[0].Err == "" {
		t.Fatalf("diagnostic = %#v, want server/tool/error", diagnostics[0])
	}
	if !session.closed {
		t.Fatal("discovery session was not closed after schema diagnostic")
	}
}

func TestExecuteCallFailureReturnsToolErrorAndCloses(t *testing.T) {
	session := &fakeSession{callErr: errors.New("server exploded")}
	client := New(map[string]config.MCPServer{"docs": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{sessions: []*fakeSession{session}}))
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{Name: "search", InputSchema: map[string]any{"type": "object"}})

	result, err := wrapper.Execute(context.Background(), json.RawMessage(`{"query":"mcp"}`))
	if err != nil {
		t.Fatalf("Execute error = %v, want nil", err)
	}
	if !result.IsError || !strings.Contains(result.Text, "server exploded") {
		t.Fatalf("Execute result = %#v, want tool-level server error", result)
	}
	if !session.closed {
		t.Fatal("call session was not closed on call error")
	}
}

func TestExecuteContextCancellationReturnsErrorAndCloses(t *testing.T) {
	session := &fakeSession{callErr: context.Canceled}
	client := New(map[string]config.MCPServer{"docs": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{sessions: []*fakeSession{session}}))
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{Name: "search", InputSchema: map[string]any{"type": "object"}})

	_, err := wrapper.Execute(context.Background(), json.RawMessage(`{"query":"mcp"}`))
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Execute error = %v, want context canceled", err)
	}
	if !session.closed {
		t.Fatal("call session was not closed on cancellation")
	}
}

func TestExecuteConnectDeadlineReturnsError(t *testing.T) {
	client := New(map[string]config.MCPServer{"docs": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{err: context.DeadlineExceeded}))
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{Name: "search", InputSchema: map[string]any{"type": "object"}})

	result, err := wrapper.Execute(context.Background(), json.RawMessage(`{"query":"mcp"}`))
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Execute error = %v, want context deadline exceeded", err)
	}
	if result.IsError || result.Text != "" {
		t.Fatalf("Execute result = %#v, want zero result for context deadline", result)
	}
}

func TestExecutePreservesTextContentAndIsError(t *testing.T) {
	session := &fakeSession{callResult: &mcp.CallToolResult{
		Content: []mcp.Content{&mcp.TextContent{Text: "first"}, &mcp.TextContent{Text: "second"}},
		IsError: true,
	}}
	client := New(map[string]config.MCPServer{"docs": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{sessions: []*fakeSession{session}}))
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{Name: "search", InputSchema: map[string]any{"type": "object"}})

	result, err := wrapper.Execute(context.Background(), json.RawMessage(`{"query":"mcp"}`))
	if err != nil {
		t.Fatalf("Execute error = %v, want nil", err)
	}
	if result.Text != "first\nsecond" || !result.IsError {
		t.Fatalf("Execute result = %#v, want concatenated text error", result)
	}
	if session.calledName != "search" {
		t.Fatalf("called name = %q, want search", session.calledName)
	}
	if !reflect.DeepEqual(session.calledInput, map[string]any{"query": "mcp"}) {
		t.Fatalf("called input = %#v, want query map", session.calledInput)
	}
	if !session.closed {
		t.Fatal("call session was not closed on success")
	}
}

func TestDiscoverFailureDoesNotFabricateTools(t *testing.T) {
	client := New(map[string]config.MCPServer{"offline": {Transport: config.MCPTransportHTTP, URL: "https://example.test/mcp"}}, WithConnector(&fakeConnector{err: errors.New("connect refused")}))

	tools, diagnostics := client.Discover(context.Background())
	if len(tools) != 0 {
		t.Fatalf("Discover tools len = %d, want 0", len(tools))
	}
	if len(diagnostics) != 1 || diagnostics[0].Server != "offline" || !strings.Contains(diagnostics[0].Err, "connect refused") {
		t.Fatalf("diagnostics = %#v, want offline connect failure", diagnostics)
	}
}

func TestTool_ReadOnly_TrueWhenServerAnnotatesReadOnlyHint(t *testing.T) {
	client := New(nil)
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{
		Name:        "search",
		InputSchema: map[string]any{"type": "object"},
		Annotations: &mcp.ToolAnnotations{ReadOnlyHint: true},
	})

	if !wrapper.ReadOnly() {
		t.Fatal("ReadOnly() = false, want true when the server sets readOnlyHint")
	}
}

func TestTool_ReadOnly_FalseWhenNoAnnotations(t *testing.T) {
	client := New(nil)
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{Name: "search", InputSchema: map[string]any{"type": "object"}})

	if wrapper.ReadOnly() {
		t.Fatal("ReadOnly() = true, want false (safe default: treat as write when no annotation is present)")
	}
}

func TestTool_ReadOnly_FalseWhenAnnotationsPresentButHintExplicitlyFalse(t *testing.T) {
	client := New(nil)
	wrapper := client.WrapDiscoveredTool("docs", &mcp.Tool{
		Name:        "search",
		InputSchema: map[string]any{"type": "object"},
		Annotations: &mcp.ToolAnnotations{ReadOnlyHint: false},
	})

	if wrapper.ReadOnly() {
		t.Fatal("ReadOnly() = true, want false when the annotation explicitly reports not read-only")
	}
}
