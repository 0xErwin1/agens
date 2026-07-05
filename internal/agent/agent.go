// Package agent is the composition root that wires a config.Config and an
// auth.File into a ready-to-run *agentloop.Loop, with no network calls of
// its own.
package agent

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"time"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	chatgptauth "github.com/iperez/agens/internal/auth/chatgpt"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/permission"
	"github.com/iperez/agens/internal/prompt"
	"github.com/iperez/agens/internal/provider"
	chatgptprovider "github.com/iperez/agens/internal/provider/chatgpt"
	"github.com/iperez/agens/internal/provider/openai"
	"github.com/iperez/agens/internal/tool"
	"github.com/iperez/agens/internal/tool/bash"
	"github.com/iperez/agens/internal/tool/fs"
	"github.com/iperez/agens/internal/tool/search"
	"github.com/iperez/agens/internal/tool/task"
	"github.com/iperez/agens/internal/tool/webfetch"
)

// defaultProviderID and chatgptProviderID identify the two providers
// BuildLoop can wire; each must match the corresponding provider's ID().
const (
	defaultProviderID = "openai-api"
	chatgptProviderID = "openai-chatgpt"
)

// Options carries the per-invocation overrides a caller (typically the
// chat command's flags) supplies on top of a loaded config.Config.
type Options struct {
	Model         string
	SystemPrompt  string
	MaxIterations int

	// ProjectRoot confines the read/write/edit tools' filesystem access. An
	// empty value falls back to os.Getwd().
	ProjectRoot string

	// Prompter resolves Ask decisions for tool calls that are not covered
	// by a static Allow/Deny rule (write and edit, by default). A nil
	// Prompter falls back to permission.DenyPrompter{}, denying every Ask.
	Prompter permission.Prompter
}

// BuildLoop resolves cfg, creds, and opts into a ready-to-run
// *agentloop.Loop. It performs no network I/O; it reads local config,
// credentials, and instruction files and constructs the provider and loop,
// both of which are pure construction.
//
// The provider to wire is resolved by selectProviderID: an explicit
// cfg.Provider.Type wins, otherwise it is inferred from which credentials
// are present in creds. The resolved model falls through opts.Model,
// cfg.Provider.Model, and finally the selected provider's own default.
func BuildLoop(cfg config.Config, creds auth.File, opts Options) (*agentloop.Loop, error) {
	providerID, err := selectProviderID(cfg, creds)
	if err != nil {
		return nil, err
	}

	model := opts.Model
	if model == "" {
		model = cfg.Provider.Model
	}
	if model == "" {
		model = defaultModelFor(providerID)
	}
	if model == "" {
		return nil, errors.New("agent: no model configured")
	}

	systemPrompt, err := BuildSystemPrompt(cfg, opts, model)
	if err != nil {
		return nil, err
	}

	p, err := buildProvider(providerID, cfg, creds, model)
	if err != nil {
		return nil, err
	}

	loopOpts := []agentloop.Option{
		agentloop.WithModel(model),
		agentloop.WithSystemPrompt(systemPrompt),
		agentloop.WithParallelToolCalls(cfg.Agent.ParallelToolCalls),
	}

	maxIterations, err := resolveMaxIterations(cfg, opts)
	if err != nil {
		return nil, err
	}
	if maxIterations > 0 {
		loopOpts = append(loopOpts, agentloop.WithMaxIterations(maxIterations))
	}

	// A subagent runs the base toolset — with no task tool, so a delegation
	// never recurses — through its own loop with an isolated conversation. The
	// task tool on the parent gate hands work to that subagent synchronously.
	subGate, err := buildGate(opts)
	if err != nil {
		return nil, err
	}
	runner := newSubagentRunner(agentloop.New(p, subGate, loopOpts...), "subagent", model)

	parentGate, err := buildParentGate(opts, runner)
	if err != nil {
		return nil, err
	}

	return agentloop.New(p, parentGate, loopOpts...), nil
}

// loopRunner is the subset of *agentloop.Loop the subagent runner drives,
// narrowed to an interface so the runner can be tested without a provider.
type loopRunner interface {
	Run(ctx context.Context, history []message.Message, sink func(agentloop.LoopEvent)) ([]message.Message, error)
}

// subagentSeq mints unique ids for subagent runs so a surface can correlate a
// delegation's lifecycle events. Subagents run one at a time (v1 is
// synchronous), but the counter is atomic so the id generation is race-free.
var subagentSeq atomic.Int64

// subagentRunner runs a delegated task through a nested loop with an isolated
// conversation, returning the subagent's final assistant text. It satisfies
// task.Runner. name and model describe the subagent to the UI panel.
type subagentRunner struct {
	loop  loopRunner
	name  string
	model string
}

func newSubagentRunner(loop loopRunner, name, model string) *subagentRunner {
	return &subagentRunner{loop: loop, name: name, model: model}
}

// Run seeds a fresh conversation with the task description and drives the
// subagent loop to completion, returning the text of its final assistant turn.
// The parent turn's ctx is threaded through, so canceling the parent cancels the
// subagent too. When the parent loop installed an event sink (WithEventSink), the
// subagent's lifecycle — start, each tool it invokes, and completion — is
// streamed as LoopSubagent* events so the UI can show it running live.
func (r *subagentRunner) Run(ctx context.Context, description string) (string, error) {
	parentEmit := agentloop.EventSink(ctx)
	id := fmt.Sprintf("subagent-%d", subagentSeq.Add(1))

	if parentEmit != nil {
		parentEmit(agentloop.LoopEvent{
			Kind:     agentloop.LoopSubagentStarted,
			Subagent: agentloop.Subagent{ID: id, Name: r.name, Model: r.model},
		})
	}

	history := []message.Message{
		message.NewMessage(message.RoleUser, message.TextPart{Text: description}),
	}

	final, err := r.loop.Run(ctx, history, subagentActivitySink(parentEmit, id))

	result := ""
	if err == nil {
		result = lastAssistantText(final)
	}
	if parentEmit != nil {
		parentEmit(agentloop.LoopEvent{
			Kind:     agentloop.LoopSubagentFinished,
			Subagent: agentloop.Subagent{ID: id, Result: result, Failed: err != nil},
		})
	}

	if err != nil {
		return "", err
	}
	return result, nil
}

// subagentActivitySink translates a subagent's own LoopEvents into the subagent
// panel's activity stream on parentEmit: each tool the subagent invokes becomes
// an activity line, and its usage becomes a running token total. It returns nil
// when there is no parent sink, so an unobserved subagent runs without overhead.
func subagentActivitySink(parentEmit func(agentloop.LoopEvent), id string) func(agentloop.LoopEvent) {
	if parentEmit == nil {
		return nil
	}

	return func(ev agentloop.LoopEvent) {
		switch ev.Kind {
		case agentloop.LoopToolCallStarted:
			parentEmit(agentloop.LoopEvent{
				Kind:     agentloop.LoopSubagentActivity,
				Subagent: agentloop.Subagent{ID: id, Activity: ev.ToolCall.Name},
			})

		case agentloop.LoopUsage:
			if ev.Usage != nil {
				parentEmit(agentloop.LoopEvent{
					Kind:     agentloop.LoopSubagentActivity,
					Subagent: agentloop.Subagent{ID: id, Tokens: ev.Usage.InputTokens + ev.Usage.OutputTokens},
				})
			}
		}
	}
}

// lastAssistantText returns the concatenated text of the most recent assistant
// message in history, or "" when there is none.
func lastAssistantText(history []message.Message) string {
	for i := len(history) - 1; i >= 0; i-- {
		if history[i].Role != message.RoleAssistant {
			continue
		}

		var b strings.Builder
		for _, part := range history[i].Parts {
			if text, ok := part.(message.TextPart); ok {
				b.WriteString(text.Text)
			}
		}
		return b.String()
	}
	return ""
}

// BuildSystemPrompt assembles the full system prompt for the resolved
// model: opts.SystemPrompt (falling back to cfg.Agent.SystemPrompt) is
// used as the base-prompt override when non-empty, otherwise the base
// prompt is chosen by prompt.Select(model); the runtime environment block
// and any discovered AGENTS.md/CLAUDE.md instructions are always appended.
// It is exported so a live model switch can rebuild the prompt (whose
// environment block names the model) for the newly selected model.
func BuildSystemPrompt(cfg config.Config, opts Options, model string) (string, error) {
	override := opts.SystemPrompt
	if override == "" {
		override = cfg.Agent.SystemPrompt
	}

	cwd, err := os.Getwd()
	if err != nil {
		return "", fmt.Errorf("agent: %w", err)
	}

	projectRoot := opts.ProjectRoot
	if projectRoot == "" {
		projectRoot = config.ProjectRoot(cwd)
	}
	_, statErr := os.Stat(filepath.Join(projectRoot, ".git"))
	isGitRepo := statErr == nil

	return prompt.Build(prompt.Options{
		Model:       model,
		Override:    override,
		WorkingDir:  cwd,
		ProjectRoot: projectRoot,
		ConfigHome:  config.HomeDir(),
		IsGitRepo:   isGitRepo,
		Platform:    runtime.GOOS,
		Now:         time.Now(),
	}), nil
}

// BuildProvider resolves cfg + creds + opts into the selected
// provider.Provider (api-key or chatgpt-oauth), without constructing the
// agent loop or tool gate. It is used by commands that only need to talk to
// the provider directly, such as listing models.
//
// Provider selection and model resolution mirror BuildLoop exactly: the
// model is resolved through opts.Model, cfg.Provider.Model, and finally
// defaultModelFor(providerID), and passed into the provider config even
// though Models does not use it, so construction never diverges from
// BuildLoop's.
func resolveMaxIterations(cfg config.Config, opts Options) (int, error) {
	if opts.MaxIterations < 0 {
		return 0, errors.New("agent: max iterations must be >= 1")
	}
	if opts.MaxIterations > 0 {
		return opts.MaxIterations, nil
	}
	if cfg.Agent.MaxIterations < 0 {
		return 0, errors.New("agent: max iterations must be >= 1")
	}
	if cfg.Agent.MaxIterations > 0 {
		return cfg.Agent.MaxIterations, nil
	}
	return 0, nil
}

func BuildProvider(cfg config.Config, creds auth.File, opts Options) (provider.Provider, error) {
	providerID, err := selectProviderID(cfg, creds)
	if err != nil {
		return nil, err
	}

	model := opts.Model
	if model == "" {
		model = cfg.Provider.Model
	}
	if model == "" {
		model = defaultModelFor(providerID)
	}
	if model == "" {
		return nil, errors.New("agent: no model configured")
	}

	return buildProvider(providerID, cfg, creds, model)
}

// ResolveModel returns the effective model name BuildLoop would run for cfg,
// creds, and opts, applying the same precedence — opts.Model, then
// cfg.Provider.Model, then the selected provider's built-in default — without
// building the loop or performing any I/O. It lets a surface display the real
// model name (e.g. the TUI status bar) instead of a placeholder.
func ResolveModel(cfg config.Config, creds auth.File, opts Options) (string, error) {
	providerID, err := selectProviderID(cfg, creds)
	if err != nil {
		return "", err
	}

	model := opts.Model
	if model == "" {
		model = cfg.Provider.Model
	}
	if model == "" {
		model = defaultModelFor(providerID)
	}
	if model == "" {
		return "", errors.New("agent: no model configured")
	}

	return model, nil
}

// selectProviderID resolves which provider id BuildLoop should construct.
// An explicit cfg.Provider.Type always wins and must name a known provider.
// Otherwise the id is inferred from creds: a well-formed ChatGPT OAuth entry
// (access token and refresh token both present) takes priority over an
// api-key entry when both exist, since a ChatGPT login is normally the more
// recently established credential.
func selectProviderID(cfg config.Config, creds auth.File) (string, error) {
	if t := cfg.Provider.Type; t != "" {
		switch t {
		case defaultProviderID, chatgptProviderID:
			return t, nil
		default:
			return "", fmt.Errorf("agent: unknown provider type %q", t)
		}
	}

	if e, ok := creds[chatgptProviderID]; ok && e.AccessToken != "" && e.RefreshToken != "" {
		return chatgptProviderID, nil
	}
	if e, ok := creds[defaultProviderID]; ok && e.APIKey != "" {
		return defaultProviderID, nil
	}
	return "", errors.New("agent: no credentials found; run 'agens auth login'")
}

// defaultModelFor returns the built-in default model for providerID, used
// when neither opts.Model nor cfg.Provider.Model specify one.
func defaultModelFor(providerID string) string {
	if providerID == chatgptProviderID {
		return chatgptprovider.DefaultModel
	}
	return openai.DefaultModel
}

// buildProvider constructs the provider.Provider named by providerID. It
// performs no network I/O: both openai.New and chatgptprovider.New are pure
// construction.
func buildProvider(providerID string, cfg config.Config, creds auth.File, model string) (provider.Provider, error) {
	switch providerID {
	case defaultProviderID:
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
		return p, nil

	case chatgptProviderID:
		entry := creds[chatgptProviderID]

		authenticator, err := chatgptauth.NewAuthenticator(entry, persistChatGPTEntry)
		if err != nil {
			return nil, fmt.Errorf("agent: %w", err)
		}

		p, err := chatgptprovider.New(provider.Config{BaseURL: cfg.Provider.BaseURL, Model: model}, authenticator)
		if err != nil {
			return nil, fmt.Errorf("agent: %w", err)
		}
		return p, nil

	default:
		return nil, fmt.Errorf("agent: unknown provider type %q", providerID)
	}
}

// persistChatGPTEntry saves a refreshed ChatGPT credential back to the
// on-disk auth file, preserving every other provider's entry: it loads the
// whole file, replaces only the "openai-chatgpt" entry, and writes the file
// back. BuildLoop's construction never invokes this itself; only a later,
// live token refresh does.
func persistChatGPTEntry(entry auth.Entry) error {
	path := auth.DefaultPath()

	file, err := auth.Load(path)
	if err != nil {
		file = auth.File{}
	}
	file[chatgptProviderID] = entry

	return auth.Save(path, file)
}

// buildGate resolves opts into a *permission.Gate wrapping the read, write,
// edit, bash, grep, glob, and webfetch tools confined to opts.ProjectRoot
// (or os.Getwd() when empty). read, grep, and glob are pre-seeded to Allow
// so they never prompt; write, edit, bash, and webfetch fall through to
// DecisionAsk by default, resolved by opts.Prompter (or
// permission.DenyPrompter{} when opts.Prompter is nil).
func buildGate(opts Options) (*permission.Gate, error) {
	reg, rules, err := baseToolset(opts)
	if err != nil {
		return nil, err
	}
	return assembleGate(reg, rules, opts)
}

// baseToolset builds the registry of tools shared by the main agent and its
// subagents — read/write/edit, bash, grep/glob, and webfetch — confined to
// opts.ProjectRoot (or the working directory when empty), together with the
// permission rules that pre-seed the read-only tools to Allow.
func baseToolset(opts Options) (*tool.Registry, []permission.Rule, error) {
	rootDir := opts.ProjectRoot
	if rootDir == "" {
		wd, err := os.Getwd()
		if err != nil {
			return nil, nil, fmt.Errorf("agent: %w", err)
		}
		rootDir = wd
	}

	dir, err := fs.Open(rootDir)
	if err != nil {
		return nil, nil, fmt.Errorf("agent: %w", err)
	}

	reg := tool.NewRegistry()
	reg.Register(fs.NewRead(dir))
	reg.Register(fs.NewWrite(dir))
	reg.Register(fs.NewEdit(dir))
	reg.Register(bash.New(rootDir))
	reg.Register(search.NewGrep(dir.FS()))
	reg.Register(search.NewGlob(dir.FS()))
	reg.Register(webfetch.New())

	rules := []permission.Rule{
		{Decision: permission.DecisionAllow, Name: "read"},
		{Decision: permission.DecisionAllow, Name: "grep"},
		{Decision: permission.DecisionAllow, Name: "glob"},
	}
	return reg, rules, nil
}

// assembleGate wraps reg and rules in a permission Gate that resolves Ask
// decisions through opts.Prompter (or DenyPrompter when nil).
func assembleGate(reg *tool.Registry, rules []permission.Rule, opts Options) (*permission.Gate, error) {
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

// buildParentGate builds the main agent's gate: the base toolset plus the task
// tool, which delegates to runner. task is pre-seeded to Allow so a delegation
// itself never prompts; the subagent's own side-effecting tool calls are still
// gated by its own gate. Subagents are built with buildGate (no task), so a
// delegation does not recurse.
func buildParentGate(opts Options, runner task.Runner) (*permission.Gate, error) {
	reg, rules, err := baseToolset(opts)
	if err != nil {
		return nil, err
	}

	reg.Register(task.New(runner))
	rules = append(rules, permission.Rule{Decision: permission.DecisionAllow, Name: "task"})

	return assembleGate(reg, rules, opts)
}
