// Package agentdef discovers and parses agent definitions: markdown files with
// a YAML frontmatter block that name an agent, describe it, choose the models
// it may run on, and carry its system prompt as the markdown body. Definitions
// come from a global directory and a project directory (project overrides
// global by name), layered over a set of generic built-ins so the feature works
// with zero files on disk.
//
// The package performs only local file I/O and parsing; wiring definitions into
// the agent loop, the task tool, or any UI is the caller's responsibility.
package agentdef

import "strings"

// Mode declares where a definition may be used: as the primary (main-thread)
// agent, as a subagent a delegation runs, or both.
type Mode string

const (
	ModePrimary  Mode = "primary"
	ModeSubagent Mode = "subagent"
	ModeAll      Mode = "all"
)

// sourceBuiltin marks a Definition that ships with the binary rather than being
// read from a file. It is surfaced in diagnostics, not compared against.
const sourceBuiltin = "builtin"

// Definition is a single agent: its name (the file stem, or the built-in id), a
// short description, the contexts it may run in, an optional default model, the
// set of models it is allowed to run on (empty means unrestricted), and its
// system prompt (the markdown body).
type Definition struct {
	Name        string
	Description string
	Mode        Mode
	Model       string
	Models      []string
	Prompt      string
	Source      string
}

// IsSubagent reports whether the definition can be run as a subagent.
func (d Definition) IsSubagent() bool {
	return d.Mode == ModeSubagent || d.Mode == ModeAll
}

// IsPrimary reports whether the definition can be run as the primary agent.
func (d Definition) IsPrimary() bool {
	return d.Mode == ModePrimary || d.Mode == ModeAll
}

// AllowsModel reports whether the definition may run on model. An empty Models
// set imposes no restriction, so every model is allowed.
func (d Definition) AllowsModel(model string) bool {
	if len(d.Models) == 0 {
		return true
	}
	for _, m := range d.Models {
		if m == model {
			return true
		}
	}
	return false
}

// Set is an ordered, name-indexed collection of definitions. Later puts under
// an existing name replace the definition in place, preserving its position, so
// project files override global files and files override built-ins without
// reordering the list a UI walks.
type Set struct {
	defs  []Definition
	index map[string]int
}

func newSet() *Set {
	return &Set{index: make(map[string]int)}
}

func (s *Set) put(d Definition) {
	if i, ok := s.index[d.Name]; ok {
		s.defs[i] = d
		return
	}
	s.index[d.Name] = len(s.defs)
	s.defs = append(s.defs, d)
}

// Upsert adds def or, when a definition of the same name already exists,
// replaces it in place, preserving its position. It lets a surface reflect an
// edited definition in the set it presents without reloading from disk.
func (s *Set) Upsert(d Definition) { s.put(d) }

// All returns a copy of every definition in insertion order.
func (s *Set) All() []Definition {
	out := make([]Definition, len(s.defs))
	copy(out, s.defs)
	return out
}

// ByName returns the definition registered under name, if any.
func (s *Set) ByName(name string) (Definition, bool) {
	i, ok := s.index[name]
	if !ok {
		return Definition{}, false
	}
	return s.defs[i], true
}

// Subagents returns the definitions that may run as a subagent, in insertion
// order.
func (s *Set) Subagents() []Definition {
	out := make([]Definition, 0, len(s.defs))
	for _, d := range s.defs {
		if d.IsSubagent() {
			out = append(out, d)
		}
	}
	return out
}

// Builtins returns the generic, domain-agnostic definitions that ship with the
// binary: a hands-on "build" agent and a read-only "plan" agent. They apply in
// both contexts (ModeAll) and place no model restriction, so a delegation can
// run either on any served model.
func Builtins() []Definition {
	return []Definition{
		{
			Name:        "build",
			Description: "Hands-on agent that implements the task end to end: reads code, edits files, runs commands.",
			Mode:        ModeAll,
			Source:      sourceBuiltin,
			Prompt: "You are a hands-on engineering subagent. Implement the delegated task from " +
				"start to finish: read the relevant code, make the necessary edits, run commands to " +
				"check your work, and iterate until it is done. Prefer the smallest change that " +
				"satisfies the task. When you finish, return a concise report of what you changed and why.",
		},
		{
			Name:        "plan",
			Description: "Read-only agent that investigates and produces a plan without editing anything.",
			Mode:        ModeAll,
			Source:      sourceBuiltin,
			Prompt: "You are a planning subagent. Investigate the delegated task and produce a clear, " +
				"actionable plan without modifying the project: read code, trace the relevant flow, and " +
				"lay out the concrete steps, files, and risks involved. Do not edit files or run " +
				"mutating commands. Return the plan as your final report.",
		},
	}
}

func cleanModels(models []string) []string {
	seen := make(map[string]bool, len(models))
	out := make([]string, 0, len(models))
	for _, m := range models {
		m = strings.TrimSpace(m)
		if m == "" || seen[m] {
			continue
		}
		seen[m] = true
		out = append(out, m)
	}
	return out
}
