package mcpclient

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strings"

	"github.com/0xErwin1/agens/internal/config"
	agenstool "github.com/0xErwin1/agens/internal/tool"
	"github.com/google/jsonschema-go/jsonschema"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type Session interface {
	ListTools(context.Context) ([]*mcp.Tool, error)
	CallTool(context.Context, *mcp.CallToolParams) (*mcp.CallToolResult, error)
	Close() error
}

type Connector interface {
	Connect(context.Context, config.MCPServer) (Session, error)
}

type Diagnostic struct {
	Server string
	Tool   string
	Err    string
}

type Client struct {
	servers   map[string]config.MCPServer
	connector Connector
}

type Option func(*Client)

func WithConnector(connector Connector) Option {
	return func(c *Client) {
		c.connector = connector
	}
}

func New(servers map[string]config.MCPServer, opts ...Option) *Client {
	copied := make(map[string]config.MCPServer, len(servers))
	for name, server := range servers {
		copied[name] = server
	}
	client := &Client{servers: copied, connector: SDKConnector{}}
	for _, opt := range opts {
		opt(client)
	}
	return client
}

func (c *Client) Discover(ctx context.Context) ([]agenstool.Tool, []Diagnostic) {
	var tools []agenstool.Tool
	var diagnostics []Diagnostic
	for _, serverName := range sortedServerNames(c.servers) {
		server := c.servers[serverName]
		session, err := c.connector.Connect(ctx, server)
		if err != nil {
			diagnostics = append(diagnostics, Diagnostic{Server: serverName, Err: err.Error()})
			continue
		}

		discovered, err := session.ListTools(ctx)
		closeErr := session.Close()
		if err != nil {
			diagnostics = append(diagnostics, Diagnostic{Server: serverName, Err: err.Error()})
			continue
		}
		if closeErr != nil {
			diagnostics = append(diagnostics, Diagnostic{Server: serverName, Err: closeErr.Error()})
			continue
		}
		for _, sdkTool := range discovered {
			wrapper := c.WrapDiscoveredTool(serverName, sdkTool)
			if _, err := wrapper.convertSchema(); err != nil {
				diagnostics = append(diagnostics, Diagnostic{Server: serverName, Tool: sdkTool.Name, Err: err.Error()})
				continue
			}
			tools = append(tools, wrapper)
		}
	}
	return tools, diagnostics
}

func (c *Client) WrapDiscoveredTool(serverName string, sdkTool *mcp.Tool) *Tool {
	return &Tool{
		serverName: serverName,
		server:     c.servers[serverName],
		sdkTool:    sdkTool,
		connector:  c.connector,
	}
}

func sortedServerNames(servers map[string]config.MCPServer) []string {
	names := make([]string, 0, len(servers))
	for name := range servers {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

type Tool struct {
	serverName string
	server     config.MCPServer
	sdkTool    *mcp.Tool
	connector  Connector
}

func (t *Tool) Name() string {
	return t.serverName + "_" + t.sdkTool.Name
}

func (t *Tool) Description() string {
	return t.sdkTool.Description
}

// ReadOnly reports whether the MCP server annotated this tool as read-only via
// its ToolAnnotations.ReadOnlyHint. A tool with no Annotations at all, or with
// Annotations present but ReadOnlyHint left at its zero value (indistinguishable
// on the wire from an explicit false), is treated as NOT read-only: the safe
// default when a server's read intent cannot be verified is to classify the
// tool as a write for chat-mode enforcement.
func (t *Tool) ReadOnly() bool {
	return t.sdkTool != nil && t.sdkTool.Annotations != nil && t.sdkTool.Annotations.ReadOnlyHint
}

func (t *Tool) Schema() *jsonschema.Schema {
	schema, err := t.convertSchema()
	if err != nil {
		return nil
	}
	return schema
}

func (t *Tool) Execute(ctx context.Context, input json.RawMessage) (agenstool.Result, error) {
	if err := ctx.Err(); err != nil {
		return agenstool.Result{}, err
	}
	var args any
	if len(input) > 0 {
		if err := json.Unmarshal(input, &args); err != nil {
			return agenstool.Result{Text: err.Error(), IsError: true}, nil
		}
	}

	session, err := t.connector.Connect(ctx, t.server)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return agenstool.Result{}, ctxErr
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return agenstool.Result{}, err
		}
		return agenstool.Result{Text: err.Error(), IsError: true}, nil
	}
	defer func() { _ = session.Close() }()

	result, err := session.CallTool(ctx, &mcp.CallToolParams{Name: t.sdkTool.Name, Arguments: args})
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return agenstool.Result{}, ctxErr
		}
		if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return agenstool.Result{}, err
		}
		return agenstool.Result{Text: err.Error(), IsError: true}, nil
	}
	return translateResult(result), nil
}

func (t *Tool) convertSchema() (*jsonschema.Schema, error) {
	if t.sdkTool == nil || t.sdkTool.InputSchema == nil {
		return nil, nil
	}
	data, err := json.Marshal(t.sdkTool.InputSchema)
	if err != nil {
		return nil, fmt.Errorf("marshal input schema: %w", err)
	}
	var schema jsonschema.Schema
	if err := json.Unmarshal(data, &schema); err != nil {
		return nil, fmt.Errorf("invalid input schema: %w", err)
	}
	if _, err := json.Marshal(&schema); err != nil {
		return nil, fmt.Errorf("invalid input schema: %w", err)
	}
	return &schema, nil
}

func translateResult(result *mcp.CallToolResult) agenstool.Result {
	if result == nil {
		return agenstool.Result{}
	}
	parts := make([]string, 0, len(result.Content)+1)
	for _, content := range result.Content {
		switch c := content.(type) {
		case *mcp.TextContent:
			parts = append(parts, c.Text)
		default:
			data, err := json.Marshal(content)
			if err != nil {
				parts = append(parts, fmt.Sprintf("%v", content))
			} else {
				parts = append(parts, string(data))
			}
		}
	}
	if result.StructuredContent != nil {
		data, err := json.Marshal(result.StructuredContent)
		if err == nil {
			parts = append(parts, string(data))
		}
	}
	return agenstool.Result{Text: strings.Join(parts, "\n"), IsError: result.IsError}
}
