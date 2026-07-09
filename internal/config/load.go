package config

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

type Scope string

const (
	ScopeGlobal  Scope = "global"
	ScopeProject Scope = "project"
)

type Config struct {
	Options     Options              `toml:"options"`
	Provider    Provider             `toml:"provider"`
	Agent       Agent                `toml:"agent"`
	UI          UI                   `toml:"ui"`
	MCP         map[string]MCPServer `toml:"mcp"`
	Permissions Permissions
}

// Permissions holds the merged [permissions] allow/deny matchers in
// scope-separated buckets rather than a single merged list. GlobalDeny is
// consumed only by the permission engine's hard pre-check layer, never by
// the last-match-wins ruleset, so a project allow can never reach or loosen
// it: applyPermissions routes each scope's patch into its own field and
// never concatenates the two scopes together.
type Permissions struct {
	GlobalAllow  []string
	GlobalDeny   []string
	ProjectAllow []string
	ProjectDeny  []string
}

const (
	MCPTransportStdio = "stdio"
	MCPTransportHTTP  = "http"
	MCPTransportSSE   = "sse"
)

type MCPServer struct {
	Transport  string            `toml:"transport"`
	Command    string            `toml:"command"`
	Args       []string          `toml:"args"`
	Env        map[string]string `toml:"env"`
	CWD        string            `toml:"cwd"`
	URL        string            `toml:"url"`
	Headers    map[string]string `toml:"headers"`
	MaxRetries int               `toml:"max_retries"`
}

// UI holds display preferences for the interactive terminal UI. Both default to
// false: a finished reasoning block is shown in full (not folded to its header)
// and an expanded tool result is shown in full (not capped). Enabling either
// restores the compact, folded/truncated behavior.
type UI struct {
	CollapseThinking   bool `toml:"collapse_thinking"`
	TruncateToolOutput bool `toml:"truncate_tool_output"`
}

type Options struct {
	Debug   bool   `toml:"debug"`
	DataDir string `toml:"data_dir"`
}

type Provider struct {
	Type    string `toml:"type"`
	Model   string `toml:"model"`
	BaseURL string `toml:"base_url"`
}

// Agent holds user-configurable overrides for the agent loop. SystemPrompt
// is a user override for the base system prompt; when empty, the base
// prompt is chosen automatically by the internal/prompt package from the
// resolved model.
type Agent struct {
	SystemPrompt      string `toml:"system_prompt"`
	MaxIterations     int    `toml:"max_iterations"`
	ParallelToolCalls bool   `toml:"parallel_tool_calls"`
}

type Source struct {
	Path  string
	Scope Scope
}

type Loaded struct {
	Config      Config
	Sources     []Source
	GlobalPath  string
	ProjectPath string
	ProjectRoot string
}

type LoadOptions struct {
	ConfigHome string
	WorkingDir string
	Env        map[string]string
}

type configPatch struct {
	Options     *optionsPatch             `toml:"options"`
	Provider    *providerPatch            `toml:"provider"`
	Agent       *agentPatch               `toml:"agent"`
	UI          *uiPatch                  `toml:"ui"`
	MCP         map[string]mcpServerPatch `toml:"mcp"`
	Permissions *permissionsPatch         `toml:"permissions"`
}

// permissionsPatch is the [permissions] block as authored in a single config
// file. Its Allow/Deny lists are matchers over native tool names (e.g.
// "bash(rm -rf *)", "read"); syntax validation is deferred to
// permission.ParseRule at composition, not performed here.
type permissionsPatch struct {
	Allow []string `toml:"allow"`
	Deny  []string `toml:"deny"`
}

type uiPatch struct {
	CollapseThinking   *bool `toml:"collapse_thinking"`
	TruncateToolOutput *bool `toml:"truncate_tool_output"`
}

type mcpServerPatch struct {
	Transport  *string           `toml:"transport"`
	Command    *string           `toml:"command"`
	Args       []string          `toml:"args"`
	Env        map[string]string `toml:"env"`
	CWD        *string           `toml:"cwd"`
	URL        *string           `toml:"url"`
	Headers    map[string]string `toml:"headers"`
	MaxRetries *int              `toml:"max_retries"`
}

type optionsPatch struct {
	Debug   *bool   `toml:"debug"`
	DataDir *string `toml:"data_dir"`
}

type providerPatch struct {
	Type    *string `toml:"type"`
	Model   *string `toml:"model"`
	BaseURL *string `toml:"base_url"`
}

type agentPatch struct {
	SystemPrompt      *string `toml:"system_prompt"`
	MaxIterations     *int    `toml:"max_iterations"`
	ParallelToolCalls *bool   `toml:"parallel_tool_calls"`
}

func DefaultConfig() Config {
	return Config{
		Options: Options{
			Debug:   false,
			DataDir: filepath.Join(defaultDataHome(), AppName),
		},
		Provider: Provider{},
		Agent: Agent{
			ParallelToolCalls: true,
		},
		MCP: map[string]MCPServer{},
	}
}

func Load() (Loaded, error) {
	cwd, err := os.Getwd()
	if err != nil {
		return Loaded{}, fmt.Errorf("get working directory: %w", err)
	}
	return LoadFrom(LoadOptions{ConfigHome: HomeDir(), WorkingDir: cwd, Env: environMap()})
}

func LoadFrom(opts LoadOptions) (Loaded, error) {
	configHome := opts.ConfigHome
	if configHome == "" {
		configHome = HomeDir()
	}
	workingDir := opts.WorkingDir
	if workingDir == "" {
		cwd, err := os.Getwd()
		if err != nil {
			return Loaded{}, fmt.Errorf("get working directory: %w", err)
		}
		workingDir = cwd
	}
	env := opts.Env
	if env == nil {
		env = environMap()
	}

	projectRoot := ProjectRoot(workingDir)
	loaded := Loaded{
		Config:      DefaultConfig(),
		GlobalPath:  filepath.Join(configHome, "config.toml"),
		ProjectRoot: projectRoot,
		ProjectPath: filepath.Join(projectRoot, ".agens", "config.toml"),
	}

	if err := loaded.applyFile(loaded.GlobalPath, ScopeGlobal, env); err != nil {
		return Loaded{}, err
	}
	if err := loaded.applyFile(loaded.ProjectPath, ScopeProject, env); err != nil {
		return Loaded{}, err
	}
	return loaded, nil
}

func ProjectRoot(start string) string {
	current, err := filepath.Abs(start)
	if err != nil {
		return start
	}
	info, err := os.Stat(current)
	if err == nil && !info.IsDir() {
		current = filepath.Dir(current)
	}
	for {
		if _, err := os.Stat(filepath.Join(current, ".git")); err == nil {
			return current
		}
		next := filepath.Dir(current)
		if next == current {
			return start
		}
		current = next
	}
}

func (l *Loaded) applyFile(path string, scope Scope, env map[string]string) error {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("%s config %s: read: %w", scope, path, err)
	}

	patch, err := decodePatch(data)
	if err != nil {
		return fmt.Errorf("%s config %s: %w", scope, path, err)
	}
	if scope == ScopeProject && len(patch.MCP) > 0 {
		return fmt.Errorf("project config %s: MCP servers must be configured in global config", path)
	}
	if err := expandPatch(&patch, env); err != nil {
		return fmt.Errorf("%s config %s: %w", scope, path, err)
	}
	if err := validatePatch(patch); err != nil {
		return fmt.Errorf("%s config %s: %w", scope, path, err)
	}
	applyPatch(&l.Config, patch)
	applyPermissions(&l.Config, patch.Permissions, scope)
	if err := validateConfig(l.Config); err != nil {
		return fmt.Errorf("%s config %s: %w", scope, path, err)
	}
	l.Sources = append(l.Sources, Source{Path: path, Scope: scope})
	return nil
}

func decodePatch(data []byte) (configPatch, error) {
	if err := checkDuplicateMCPTables(data); err != nil {
		return configPatch{}, err
	}
	var patch configPatch
	decoder := toml.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&patch); err != nil {
		return configPatch{}, fmt.Errorf("invalid TOML: %w", err)
	}
	return patch, nil
}

func checkDuplicateMCPTables(data []byte) error {
	seen := map[string]struct{}{}
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "[mcp.") || !strings.HasSuffix(line, "]") || strings.HasPrefix(line, "[[") {
			continue
		}
		name := strings.TrimSpace(strings.TrimSuffix(strings.TrimPrefix(line, "[mcp."), "]"))
		if _, ok := seen[name]; ok {
			return fmt.Errorf("duplicate MCP server %s", "mcp."+name)
		}
		seen[name] = struct{}{}
	}
	return nil
}

func environMap() map[string]string {
	result := make(map[string]string)
	for _, item := range os.Environ() {
		key, value, ok := splitEnv(item)
		if ok {
			result[key] = value
		}
	}
	return result
}

func splitEnv(item string) (string, string, bool) {
	for i, ch := range item {
		if ch == '=' {
			return item[:i], item[i+1:], true
		}
	}
	return "", "", false
}

func defaultDataHome() string {
	if value := os.Getenv("XDG_DATA_HOME"); value != "" {
		return value
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(".", ".local", "share")
	}
	return filepath.Join(home, ".local", "share")
}
