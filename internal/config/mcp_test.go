package config

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestLoadMCPServersValidateTransportSelection(t *testing.T) {
	tests := []struct {
		name    string
		config  string
		wantErr string
	}{
		{
			name: "valid stdio http and sse servers",
			config: `[mcp.files]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]

[mcp.docs]
transport = "http"
url = "https://mcp.example.test/mcp"
headers = { Authorization = "Bearer token" }
max_retries = 2

[mcp.events]
transport = "sse"
url = "https://mcp.example.test/sse"
`,
		},
		{
			name: "missing transport",
			config: `[mcp.files]
command = "npx"
`,
			wantErr: `mcp.files.transport is required`,
		},
		{
			name: "unknown transport",
			config: `[mcp.files]
transport = "websocket"
url = "wss://mcp.example.test"
`,
			wantErr: `mcp.files.transport must be one of stdio, http, or sse`,
		},
		{
			name: "ambiguous stdio and http fields",
			config: `[mcp.files]
transport = "stdio"
command = "npx"
url = "https://mcp.example.test/mcp"
`,
			wantErr: `mcp.files selects stdio but also sets http/sse fields`,
		},
		{
			name: "http with stdio fields",
			config: `[mcp.docs]
transport = "http"
url = "https://mcp.example.test/mcp"
command = "npx"
`,
			wantErr: `mcp.docs selects http but also sets stdio fields`,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			home := t.TempDir()
			if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(tt.config), 0o644); err != nil {
				t.Fatal(err)
			}

			loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
			if tt.wantErr != "" {
				if err == nil {
					t.Fatal("LoadFrom() error = nil, want error")
				}
				if !strings.Contains(err.Error(), tt.wantErr) {
					t.Fatalf("LoadFrom() error = %q, want it to contain %q", err.Error(), tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("LoadFrom() error = %v", err)
			}
			if len(loaded.Config.MCP) != 3 {
				t.Fatalf("MCP servers len = %d, want 3", len(loaded.Config.MCP))
			}
			if loaded.Config.MCP["files"].Transport != MCPTransportStdio {
				t.Fatalf("files transport = %q, want stdio", loaded.Config.MCP["files"].Transport)
			}
		})
	}
}

func TestLoadRejectsDuplicateMCPServerNames(t *testing.T) {
	home := t.TempDir()
	config := `[mcp.files]
transport = "stdio"
command = "npx"

[mcp.files]
transport = "http"
url = "https://mcp.example.test/mcp"
`
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(config), 0o644); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err == nil {
		t.Fatal("LoadFrom() error = nil, want duplicate server name error")
	}
	if !strings.Contains(err.Error(), "mcp.files") {
		t.Fatalf("LoadFrom() error = %q, want it to mention mcp.files", err.Error())
	}
}

func TestLoadExpandsAllMCPStringFields(t *testing.T) {
	home := t.TempDir()
	config := `[mcp.files]
transport = "stdio"
command = "${MCP_BIN:-printf}"
args = ["$MCP_ARG", "$(printf cmd-out)"]
cwd = "$MCP_CWD"
env = { TOKEN = "${MCP_TOKEN:-fallback}", FROM_CMD = "$(printf 'env-out\\n')" }
`
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(config), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{
		ConfigHome: home,
		WorkingDir: t.TempDir(),
		Env: map[string]string{
			"MCP_ARG": "arg-value",
			"MCP_CWD": "/tmp/mcp",
		},
	})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	server := loaded.Config.MCP["files"]
	if server.Command != "printf" {
		t.Fatalf("Command = %q, want expanded default", server.Command)
	}
	if got, want := strings.Join(server.Args, ","), "arg-value,cmd-out"; got != want {
		t.Fatalf("Args = %q, want %q", got, want)
	}
	if server.CWD != "/tmp/mcp" {
		t.Fatalf("CWD = %q, want /tmp/mcp", server.CWD)
	}
	if server.Env["TOKEN"] != "fallback" {
		t.Fatalf("Env TOKEN = %q, want fallback", server.Env["TOKEN"])
	}
	if server.Env["FROM_CMD"] != "env-out" {
		t.Fatalf("Env FROM_CMD = %q, want trimmed command output", server.Env["FROM_CMD"])
	}
}

func TestLoadExpandsMCPHTTPStringFields(t *testing.T) {
	home := t.TempDir()
	config := `[mcp.docs]
transport = "http"
url = "https://$MCP_HOST/mcp?token=$(printf token)"
headers = { Authorization = "Bearer ${MCP_TOKEN:-fallback}" }
`
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(config), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{
		ConfigHome: home,
		WorkingDir: t.TempDir(),
		Env:        map[string]string{"MCP_HOST": "mcp.example.test"},
	})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	server := loaded.Config.MCP["docs"]
	if server.URL != "https://mcp.example.test/mcp?token=token" {
		t.Fatalf("URL = %q, want expanded URL", server.URL)
	}
	if server.Headers["Authorization"] != "Bearer fallback" {
		t.Fatalf("Authorization = %q, want expanded fallback", server.Headers["Authorization"])
	}
}

func TestLoadRejectsMCPCommandSubstitutionFailure(t *testing.T) {
	home := t.TempDir()
	config := `[mcp.files]
transport = "stdio"
command = "$(sh -c 'echo boom >&2; exit 7')"
`
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(config), 0o644); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err == nil {
		t.Fatal("LoadFrom() error = nil, want command substitution error")
	}
	if !strings.Contains(err.Error(), "command substitution") || !strings.Contains(err.Error(), "exit status 7") {
		t.Fatalf("LoadFrom() error = %q, want command substitution exit status", err.Error())
	}
}

func TestExpandEnvDoesNotRecurseIntoCommandOutput(t *testing.T) {
	got, err := ExpandEnvWithCommands("$(printf '$MCP_TOKEN\\n')", map[string]string{"MCP_TOKEN": "secret"})
	if err != nil {
		t.Fatalf("ExpandEnvWithCommands() error = %v", err)
	}
	if got != "$MCP_TOKEN" {
		t.Fatalf("ExpandEnvWithCommands() = %q, want literal command output", got)
	}
}

func TestLoadRejectsProjectLocalMCPServers(t *testing.T) {
	home := t.TempDir()
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	projectConfigDir := filepath.Join(repo, ".agens")
	if err := os.Mkdir(projectConfigDir, 0o755); err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(t.TempDir(), "project-mcp-command-ran")
	project := `[mcp.files]
transport = "stdio"
command = "$(touch ` + marker + `)"
`
	if err := os.WriteFile(filepath.Join(projectConfigDir, "config.toml"), []byte(project), 0o644); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: repo, Env: map[string]string{}})
	if err == nil {
		t.Fatal("LoadFrom() error = nil, want project MCP rejection")
	}
	if !strings.Contains(err.Error(), "project config") || !strings.Contains(err.Error(), "MCP servers") || !strings.Contains(err.Error(), "global config") {
		t.Fatalf("LoadFrom() error = %q, want project MCP rejection with global config guidance", err.Error())
	}
	if _, statErr := os.Stat(marker); !errors.Is(statErr, os.ErrNotExist) {
		t.Fatalf("project MCP command substitution created %s; stat error = %v", marker, statErr)
	}
}
