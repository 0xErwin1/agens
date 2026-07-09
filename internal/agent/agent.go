// Package agent is the composition root that wires a config.Config and an
// auth.File into a ready-to-run *agentloop.Loop.
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

	"github.com/0xErwin1/agens/internal/agentdef"
	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/auth"
	chatgptauth "github.com/0xErwin1/agens/internal/auth/chatgpt"
	"github.com/0xErwin1/agens/internal/config"
	"github.com/0xErwin1/agens/internal/mcpclient"
	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/permission"
	"github.com/0xErwin1/agens/internal/prompt"
	"github.com/0xErwin1/agens/internal/provider"
	chatgptprovider "github.com/0xErwin1/agens/internal/provider/chatgpt"
	"github.com/0xErwin1/agens/internal/provider/openai"
	"github.com/0xErwin1/agens/internal/skill"
	"github.com/0xErwin1/agens/internal/tool"
	"github.com/0xErwin1/agens/internal/tool/bash"
	"github.com/0xErwin1/agens/internal/tool/fs"
	"github.com/0xErwin1/agens/internal/tool/search"
	"github.com/0xErwin1/agens/internal/tool/task"
	"github.com/0xErwin1/agens/internal/tool/webfetch"
)

// defaultProviderID and chatgptProviderID identify the two providers
// BuildLoop can wire; each must match the corresponding provider's ID().
const (
	defaultProviderID   = "openai-api"
	chatgptProviderID   = "openai-chatgpt"
	mcpDiscoveryTimeout = 10 * time.Second
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

	// Permissions carries the scope-separated [permissions] config buckets
	// assembleGate composes into the gate's ruleset: GlobalAllow,
	// ProjectAllow, and ProjectDeny become static rules (in that order, so a
	// project deny outranks a project allow under last-match-wins);
	// GlobalDeny is fed to permission.WithGlobalDenies, an absolute hard
	// pre-check no allow rule — static or persisted — can reach.
	Permissions config.Permissions

	// PermissionStore backs the gate's remembered allow/deny-always answers.
	// A nil value falls back to a fresh permission.MemoryStore, so grants
	// live only for the process's lifetime — the behavior every existing
	// caller and test relies on. A caller wanting grants to survive a
	// restart supplies a *permissiondb.Store opened for the invoking
	// project.
	PermissionStore permission.Store

	// Subagents, when non-nil, is the shared catalog of selectable subagents the
	// task tool reads. BuildLoop populates it from the discovered definitions, so
	// a surface holding the same catalog (the TUI's agents menu) can change the
	// models a delegation may pick and have it take effect on the next turn. A nil
	// value makes BuildLoop use a private catalog, fixed for the session.
	Subagents *task.Catalog

	// Skills, when non-nil, is the discovered skill set the parent agent's
	// system prompt advertises (level 1) and the skill tool reads (levels 2-3).
	// A surface loads it once and passes it here so a live prompt rebuild keeps
	// the same skills; a nil value makes BuildLoop discover them itself.
	Skills *skill.Set

	mcpServers    map[string]config.MCPServer
	mcpDiscoverer mcpToolDiscoverer
}

// BuildLoop resolves cfg, creds, and opts into a ready-to-run
// *agentloop.Loop. It reads local config, credentials, and instruction files,
// constructs the provider and loop, and discovers configured MCP tools for the
// initial registry.
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

	// Skills are a parent-only surface: their level-1 block goes into the parent
	// prompt and the skill tool into the parent gate. Subagents run the base
	// toolset with no skill tool, so their prompts (built from opts below) must
	// not carry the skills block — opts keeps Skills nil for them, and only the
	// parent prompt is built from an opts copy that has them.
	skills := opts.Skills
	if skills == nil {
		skills, _, err = LoadSkills(opts)
		if err != nil {
			return nil, err
		}
	}

	systemPrompt, err := BuildSystemPrompt(cfg, withSkills(opts, skills), model)
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

	// A subagent runs the base toolset — with no task tool, so a delegation never
	// recurses — through its own loop with an isolated conversation, the system
	// prompt of the chosen agent definition, and the model picked for the
	// delegation. buildSub constructs that loop per delegation, since the model
	// (and thus the prompt's environment block) varies by task.
	// Definition-file warnings are surfaced by the interactive/headless entry
	// points that load defs directly (the TUI and the chat command); BuildLoop
	// only needs the resolved set.
	defs, _, err := LoadAgentDefs(opts)
	if err != nil {
		return nil, err
	}

	toolOpts := opts
	toolOpts.mcpServers = cfg.MCP
	toolOpts.Permissions = cfg.Permissions

	// The subagent gate (base toolset, no task) depends on neither the delegated
	// agent nor the model, so it is built once and shared across delegations,
	// which run one at a time. Building it per delegation would leak an os.Root
	// file descriptor each time, since the confinement root has no Close.
	subGate, err := buildGate(toolOpts)
	if err != nil {
		return nil, err
	}

	buildSub := func(def agentdef.Definition, subModel string) (loopRunner, error) {
		// A subagent has no skill tool, so its prompt must not advertise skills;
		// withSystemPrompt carries the parent's opts, which may hold a skill set.
		subPromptOpts := withSystemPrompt(opts, def.Prompt)
		subPromptOpts.Skills = nil

		subPrompt, err := BuildSystemPrompt(cfg, subPromptOpts, subModel)
		if err != nil {
			return nil, err
		}

		subOpts := []agentloop.Option{
			agentloop.WithModel(subModel),
			agentloop.WithSystemPrompt(subagentSystemPrompt(subPrompt)),
			agentloop.WithParallelToolCalls(cfg.Agent.ParallelToolCalls),
		}
		if maxIterations > 0 {
			subOpts = append(subOpts, agentloop.WithMaxIterations(maxIterations))
		}

		return agentloop.New(p, subGate, subOpts...), nil
	}

	// The task tool reads its selectable subagents from a shared catalog: an
	// opts-provided one (so a surface can edit it live) or a private one. Either
	// way it is (re)seeded from the discovered definitions.
	catalog := opts.Subagents
	if catalog == nil {
		catalog = task.NewCatalog(nil)
	}
	catalog.Replace(subagentOptions(defs))

	runner := newSubagentRunner(buildSub, defs, catalog, model)

	parentGate, err := buildParentGate(toolOpts, runner, catalog, skills)
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

// subagentSystemPrompt derives a subagent's system prompt from the main prompt,
// appending an instruction that it works autonomously on a single delegated task
// and returns a concise final report, since it cannot delegate further. This is
// a v1 stand-in until agent-defs (a dedicated subagent definition) lands.
func subagentSystemPrompt(base string) string {
	const instruction = "You are running as a subagent handling one delegated task with your own " +
		"isolated context. Work autonomously to complete it, then return a single, concise " +
		"final report of what you did and what you found. You cannot delegate to further subagents."

	if base == "" {
		return instruction
	}
	return base + "\n\n" + instruction
}

// subagentSeq mints unique ids for subagent runs so a surface can correlate a
// delegation's lifecycle events. Subagents run one at a time (v1 is
// synchronous), but the counter is atomic so the id generation is race-free.
var subagentSeq atomic.Int64

// subLoopBuilder constructs a subagent loop for a resolved definition and model.
// It returns the loopRunner interface (satisfied by *agentloop.Loop) so the
// runner can be driven with a double in tests.
type subLoopBuilder func(def agentdef.Definition, model string) (loopRunner, error)

// subagentRunner runs a delegated task through a nested loop with an isolated
// conversation, returning the subagent's final assistant text. It satisfies
// task.Runner. The definition and model are resolved per delegation from the
// request; build then constructs the loop for that pair.
type subagentRunner struct {
	build       subLoopBuilder
	defs        *agentdef.Set
	catalog     *task.Catalog
	parentModel string
}

func newSubagentRunner(build subLoopBuilder, defs *agentdef.Set, catalog *task.Catalog, parentModel string) *subagentRunner {
	return &subagentRunner{build: build, defs: defs, catalog: catalog, parentModel: parentModel}
}

// Run resolves the request to a definition and model, seeds a fresh conversation
// with the task description, and drives the subagent loop to completion,
// returning the text of its final assistant turn. The parent turn's ctx is
// threaded through, so canceling the parent cancels the subagent too. When the
// parent loop installed an event sink (WithEventSink), the subagent's lifecycle
// — start, each tool it invokes, and completion — is streamed as LoopSubagent*
// events so the UI can show it running live.
func (r *subagentRunner) Run(ctx context.Context, req task.Request) (string, error) {
	def, model := r.resolve(req)

	parentEmit := agentloop.EventSink(ctx)
	id := fmt.Sprintf("subagent-%d", subagentSeq.Add(1))

	if parentEmit != nil {
		parentEmit(agentloop.LoopEvent{
			Kind:     agentloop.LoopSubagentStarted,
			Subagent: agentloop.Subagent{ID: id, Name: def.Name, Model: model, Prompt: req.Description},
		})
	}

	loop, err := r.build(def, model)
	if err != nil {
		if parentEmit != nil {
			parentEmit(agentloop.LoopEvent{
				Kind:     agentloop.LoopSubagentFinished,
				Subagent: agentloop.Subagent{ID: id, Failed: true},
			})
		}
		return "", err
	}

	history := []message.Message{
		message.NewMessage(message.RoleUser, message.TextPart{Text: req.Description}),
	}

	final, err := loop.Run(ctx, history, subagentActivitySink(parentEmit, id))

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

// resolve picks the definition and model for a request: the named definition
// (or the first subagent-capable one, or a bare fallback when none exist), and
// the request's model, falling back to the definition's default model and then
// the parent's model.
func (r *subagentRunner) resolve(req task.Request) (agentdef.Definition, string) {
	def, ok := r.defs.ByName(req.Agent)
	if !ok {
		if subs := r.defs.Subagents(); len(subs) > 0 {
			def = subs[0]
		} else {
			def = agentdef.Definition{Name: "subagent", Mode: agentdef.ModeAll}
		}
	}

	model := req.Model
	if model == "" {
		model = def.Model
	}
	if model == "" {
		model = r.parentModel
	}

	// Enforce the agent's allow-list on the resolved model, so an omitted or
	// parent-inherited model never runs the agent on a model it disallows — the
	// task tool only validates an explicitly requested model. The allowed set is
	// read from the live catalog, matching the tool's own validation source.
	if allowed := r.allowedModels(def.Name); len(allowed) > 0 && !containsString(allowed, model) {
		model = allowed[0]
	}

	return def, model
}

// allowedModels returns the models the named agent may run on, from the live
// catalog (nil when there is no catalog or no such agent, meaning unrestricted).
func (r *subagentRunner) allowedModels(name string) []string {
	if r.catalog == nil {
		return nil
	}
	for _, a := range r.catalog.Agents() {
		if a.Name == name {
			return a.Models
		}
	}
	return nil
}

func containsString(items []string, want string) bool {
	for _, s := range items {
		if s == want {
			return true
		}
	}
	return false
}

// subagentActivitySink forwards each of a subagent's own LoopEvents to parentEmit,
// wrapped as a LoopSubagentActivity tagged with the subagent id and carrying the
// event verbatim, so a surface can both drive a compact panel and render the
// subagent's full conversation like the main thread. It returns nil when there is
// no parent sink, so an unobserved subagent runs without overhead.
func subagentActivitySink(parentEmit func(agentloop.LoopEvent), id string) func(agentloop.LoopEvent) {
	if parentEmit == nil {
		return nil
	}

	return func(ev agentloop.LoopEvent) {
		forwarded := ev
		parentEmit(agentloop.LoopEvent{
			Kind:     agentloop.LoopSubagentActivity,
			Subagent: agentloop.Subagent{ID: id, Event: &forwarded},
		})
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
		Skills:      skillInfos(opts.Skills),
	}), nil
}

// skillInfos projects a skill set into the level-1 prompt disclosure — each
// skill's name and description. A nil set yields no infos, so the prompt's
// skills section is dropped entirely.
func skillInfos(set *skill.Set) []prompt.SkillInfo {
	if set == nil {
		return nil
	}
	skills := set.All()
	infos := make([]prompt.SkillInfo, 0, len(skills))
	for _, s := range skills {
		infos = append(infos, prompt.SkillInfo{Name: s.Name, Description: s.Description})
	}
	return infos
}

// withSkills returns a copy of opts with Skills set, used to build the parent
// agent's prompt with the discovered skills while leaving the caller's opts
// (and thus the subagent prompts built from it) skill-free.
func withSkills(opts Options, skills *skill.Set) Options {
	opts.Skills = skills
	return opts
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
type mcpToolDiscoverer interface {
	Discover(context.Context) ([]tool.Tool, []mcpclient.Diagnostic)
}

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
	if err := registerMCPTools(context.Background(), reg, opts); err != nil {
		return nil, nil, err
	}

	rules := []permission.Rule{
		{Decision: permission.DecisionAllow, Name: "read"},
		{Decision: permission.DecisionAllow, Name: "grep"},
		{Decision: permission.DecisionAllow, Name: "glob"},
	}
	return reg, rules, nil
}

func registerMCPTools(ctx context.Context, reg *tool.Registry, opts Options) error {
	discoverer := opts.mcpDiscoverer
	if discoverer == nil && len(opts.mcpServers) > 0 {
		discoverer = mcpclient.New(opts.mcpServers)
	}
	if discoverer == nil {
		return nil
	}

	discoverCtx, cancel := context.WithTimeout(ctx, mcpDiscoveryTimeout)
	defer cancel()
	tools, _ := discoverer.Discover(discoverCtx)
	seen := map[string]struct{}{}
	for _, t := range tools {
		name := t.Name()
		if _, ok := seen[name]; ok {
			return fmt.Errorf("agent: duplicate MCP tool name %q", name)
		}
		seen[name] = struct{}{}
	}
	for _, t := range tools {
		reg.Register(t)
	}
	return nil
}

// assembleGate wraps reg and rules in a permission Gate that resolves Ask
// decisions through opts.Prompter (or DenyPrompter when nil). opts.Permissions
// is parsed into the Engine's static rules and its global-deny hard
// pre-check by configPermissionRules. The Engine's remembered-grant Store is
// opts.PermissionStore, or a fresh permission.MemoryStore when nil.
func assembleGate(reg *tool.Registry, rules []permission.Rule, opts Options) (*permission.Gate, error) {
	rules, globalDenies, err := configPermissionRules(rules, opts.Permissions)
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	var engineOpts []permission.EngineOption
	if len(globalDenies) > 0 {
		engineOpts = append(engineOpts, permission.WithGlobalDenies(globalDenies))
	}

	store := opts.PermissionStore
	if store == nil {
		store = permission.NewMemoryStore()
	}

	engine, err := permission.NewEngine(rules, store, engineOpts...)
	if err != nil {
		return nil, fmt.Errorf("agent: %w", err)
	}

	prompter := opts.Prompter
	if prompter == nil {
		prompter = permission.DenyPrompter{}
	}

	return permission.NewGate(reg, engine, prompter), nil
}

// configPermissionRules appends perms' GlobalAllow, ProjectAllow, and
// ProjectDeny buckets onto rules — ProjectDeny last, so within its own
// scope a deny outranks an allow under last-match-wins — and parses perms'
// GlobalDeny bucket into a separate slice for WithGlobalDenies. It returns
// the first error encountered parsing any bucket, naming the offending
// matcher, so a broken [permissions] entry fails composition loudly rather
// than being silently dropped.
func configPermissionRules(rules []permission.Rule, perms config.Permissions) ([]permission.Rule, []permission.Rule, error) {
	globalAllow, err := permission.ParseRules(perms.GlobalAllow, permission.DecisionAllow)
	if err != nil {
		return nil, nil, err
	}
	projectAllow, err := permission.ParseRules(perms.ProjectAllow, permission.DecisionAllow)
	if err != nil {
		return nil, nil, err
	}
	projectDeny, err := permission.ParseRules(perms.ProjectDeny, permission.DecisionDeny)
	if err != nil {
		return nil, nil, err
	}
	globalDeny, err := permission.ParseRules(perms.GlobalDeny, permission.DecisionDeny)
	if err != nil {
		return nil, nil, err
	}

	merged := make([]permission.Rule, 0, len(rules)+len(globalAllow)+len(projectAllow)+len(projectDeny))
	merged = append(merged, rules...)
	merged = append(merged, globalAllow...)
	merged = append(merged, projectAllow...)
	merged = append(merged, projectDeny...)

	return merged, globalDeny, nil
}

// buildParentGate builds the main agent's gate: the base toolset plus the task
// tool, which delegates to runner and offers the subagents of catalog as its
// selectable agents, plus the skill tool when skills is non-empty. task and
// skill are pre-seeded to Allow so neither prompts; a delegation's own
// side-effecting tool calls are still gated by the subagent's gate, and the
// skill tool is read-only and confined to each skill's directory. Subagents are
// built with buildGate (no task, no skill), so a delegation does not recurse and
// cannot load skills.
func buildParentGate(opts Options, runner task.Runner, catalog *task.Catalog, skills *skill.Set) (*permission.Gate, error) {
	reg, rules, err := baseToolset(opts)
	if err != nil {
		return nil, err
	}

	reg.Register(task.New(runner, catalog))
	rules = append(rules, permission.Rule{Decision: permission.DecisionAllow, Name: "task"})

	if skills != nil && skills.Len() > 0 {
		reg.Register(skill.NewTool(skills))
		rules = append(rules, permission.Rule{Decision: permission.DecisionAllow, Name: "skill"})
	}

	return assembleGate(reg, rules, opts)
}

// subagentOptions projects the subagent-capable definitions of defs into the
// task.Agent options the task tool advertises and validates against.
func subagentOptions(defs *agentdef.Set) []task.Agent {
	subs := defs.Subagents()
	out := make([]task.Agent, 0, len(subs))
	for _, d := range subs {
		out = append(out, task.Agent{Name: d.Name, Description: d.Description, Models: d.Models})
	}
	return out
}

// LoadAgentDefs discovers the agent definitions available for opts: the built-in
// generic agents overlaid by the files in the global agents directory and then
// the project's .agens/agents directory. A malformed or unreadable definition
// file is skipped and returned as a human-readable warning rather than failing,
// so one bad file never blocks startup; the error return is reserved for a
// failure to resolve the project root. It is exported so a surface (the TUI's
// agents menu) can present and edit the same definitions the loop resolves
// delegations against, and surface the same warnings.
func LoadAgentDefs(opts Options) (*agentdef.Set, []string, error) {
	projectRoot := opts.ProjectRoot
	if projectRoot == "" {
		wd, err := os.Getwd()
		if err != nil {
			return nil, nil, fmt.Errorf("agent: %w", err)
		}
		projectRoot = config.ProjectRoot(wd)
	}

	globalDir := filepath.Join(config.HomeDir(), "agents")
	projectDir := filepath.Join(projectRoot, ".agens", "agents")

	set, issues := agentdef.Load(globalDir, projectDir)

	warnings := make([]string, 0, len(issues))
	for _, issue := range issues {
		warnings = append(warnings, issue.Error())
	}

	return set, warnings, nil
}

// LoadSkills discovers the Agent Skills available for opts: the skill
// directories under the global skills directory overlaid by the project's
// .agens/skills directory, a project skill shadowing a global one of the same
// name. A malformed or unreadable skill is skipped and returned as a
// human-readable warning rather than failing, so one bad skill never blocks
// startup; the error return is reserved for a failure to resolve the project
// root. It is exported so a surface can load the set once and pass it into
// Options.Skills, keeping a live prompt rebuild on the same skills.
func LoadSkills(opts Options) (*skill.Set, []string, error) {
	projectRoot := opts.ProjectRoot
	if projectRoot == "" {
		wd, err := os.Getwd()
		if err != nil {
			return nil, nil, fmt.Errorf("agent: %w", err)
		}
		projectRoot = config.ProjectRoot(wd)
	}

	globalDir := filepath.Join(config.HomeDir(), "skills")
	projectDir := filepath.Join(projectRoot, ".agens", "skills")

	set, issues := skill.Load(globalDir, projectDir)

	warnings := make([]string, 0, len(issues))
	for _, issue := range issues {
		warnings = append(warnings, issue.Error())
	}

	return set, warnings, nil
}

// withSystemPrompt returns a copy of opts with SystemPrompt set to prompt, used
// to build a subagent's prompt from its definition body while reusing the rest
// of the parent's options.
func withSystemPrompt(opts Options, prompt string) Options {
	opts.SystemPrompt = prompt
	return opts
}
