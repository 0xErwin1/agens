// Package agent is the composition root that wires a config.Config and an
// auth.File into a ready-to-run *agentloop.Loop, with no network calls of
// its own.
package agent

import (
	"errors"
	"fmt"
	"os"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/permission"
	"github.com/iperez/agens/internal/provider"
	"github.com/iperez/agens/internal/provider/openai"
	"github.com/iperez/agens/internal/tool"
	"github.com/iperez/agens/internal/tool/fs"
)

// defaultProviderID is the only provider currently wired: it must match
// (*openai.Provider).ID().
const defaultProviderID = "openai-api"

// Options carries the per-invocation overrides a caller (typically the
// chat command's flags) supplies on top of a loaded config.Config.
type Options struct {
	Model        string
	SystemPrompt string

	// ProjectRoot confines the read/write/edit tools' filesystem access. An
	// empty value falls back to os.Getwd().
	ProjectRoot string

	// Prompter resolves Ask decisions for tool calls that are not covered
	// by a static Allow/Deny rule (write and edit, by default). A nil
	// Prompter falls back to permission.DenyPrompter{}, denying every Ask.
	Prompter permission.Prompter
}

// BuildLoop resolves cfg, creds, and opts into a ready-to-run
// *agentloop.Loop. It performs no network I/O: only openai.New and
// agentloop.New are called, both of which are pure construction.
func BuildLoop(cfg config.Config, creds auth.File, opts Options) (*agentloop.Loop, error) {
	model := opts.Model
	if model == "" {
		model = cfg.Provider.Model
	}
	if model == "" {
		return nil, errors.New("agent: no model configured")
	}

	systemPrompt := opts.SystemPrompt
	if systemPrompt == "" {
		systemPrompt = cfg.Agent.SystemPrompt
	}

	key, err := creds.APIKey(defaultProviderID)
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	authenticator, err := openai.NewAPIKeyAuthenticator(key)
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	p, err := openai.New(provider.Config{BaseURL: cfg.Provider.BaseURL, Model: model}, authenticator)
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	gate, err := buildGate(opts)
	if err != nil {
		return nil, err
	}

	loopOpts := []agentloop.Option{agentloop.WithModel(model)}
	if systemPrompt != "" {
		loopOpts = append(loopOpts, agentloop.WithSystemPrompt(systemPrompt))
	}

	return agentloop.New(p, gate, loopOpts...), nil
}

// buildGate resolves opts into a *permission.Gate wrapping the read, write,
// and edit tools confined to opts.ProjectRoot (or os.Getwd() when empty).
// read is pre-seeded to Allow so it never prompts; write and edit fall
// through to DecisionAsk by default, resolved by opts.Prompter (or
// permission.DenyPrompter{} when opts.Prompter is nil).
func buildGate(opts Options) (*permission.Gate, error) {
	rootDir := opts.ProjectRoot
	if rootDir == "" {
		wd, err := os.Getwd()
		if err != nil {
			return nil, fmt.Errorf("agent: %w", err)
		}
		rootDir = wd
	}

	dir, err := fs.Open(rootDir)
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	reg := tool.NewRegistry()
	reg.Register(fs.NewRead(dir))
	reg.Register(fs.NewWrite(dir))
	reg.Register(fs.NewEdit(dir))

	rules := []permission.Rule{{Decision: permission.DecisionAllow, Name: "read"}}
	engine, err := permission.NewEngine(rules, permission.NewMemoryStore())
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	prompter := opts.Prompter
	if prompter == nil {
		prompter = permission.DenyPrompter{}
	}

	return permission.NewGate(reg, engine, prompter), nil
}
