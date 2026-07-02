// Package agent is the composition root that wires a config.Config and an
// auth.File into a ready-to-run *agentloop.Loop, with no network calls of
// its own.
package agent

import (
	"errors"
	"fmt"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/provider"
	"github.com/iperez/agens/internal/provider/openai"
	"github.com/iperez/agens/internal/tool"
)

// defaultProviderID is the only provider currently wired: it must match
// (*openai.Provider).ID().
const defaultProviderID = "openai-api"

// Options carries the per-invocation overrides a caller (typically the
// chat command's flags) supplies on top of a loaded config.Config.
type Options struct {
	Model        string
	SystemPrompt string
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

	reg := tool.NewRegistry()

	loopOpts := []agentloop.Option{agentloop.WithModel(model)}
	if systemPrompt != "" {
		loopOpts = append(loopOpts, agentloop.WithSystemPrompt(systemPrompt))
	}

	return agentloop.New(p, reg, loopOpts...), nil
}
