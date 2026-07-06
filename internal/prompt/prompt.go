package prompt

import (
	"strings"
	"time"
)

type Options struct {
	Model       string
	Override    string
	WorkingDir  string
	ProjectRoot string
	ConfigHome  string
	IsGitRepo   bool
	Platform    string
	Now         time.Time

	// Skills carries the level-1 disclosure (name + description) of each
	// discovered skill. When non-empty it is appended as a final section so the
	// model knows which skills it can load; when empty no section is added.
	Skills []SkillInfo
}

// Build assembles the full system prompt: the base persona (Override if
// set, else Select(Model)) followed by the environment block, followed by
// any discovered instruction files, all joined by blank lines. Empty parts
// are dropped; the result is "" only if every part is empty.
func Build(o Options) string {
	base := o.Override
	if base == "" {
		base = Select(o.Model)
	}

	env := Environment(Env{
		Model:       o.Model,
		WorkingDir:  o.WorkingDir,
		ProjectRoot: o.ProjectRoot,
		IsGitRepo:   o.IsGitRepo,
		Platform:    o.Platform,
		Now:         o.Now,
	})

	parts := make([]string, 0, 3+len(projectInstructionFilenames))
	parts = append(parts, base, env)
	parts = append(parts, Instructions(o.ConfigHome, o.WorkingDir, o.ProjectRoot)...)
	parts = append(parts, skillsSection(o.Skills))

	return joinNonEmpty(parts)
}

func joinNonEmpty(parts []string) string {
	nonEmpty := make([]string, 0, len(parts))
	for _, part := range parts {
		if part != "" {
			nonEmpty = append(nonEmpty, part)
		}
	}
	return strings.Join(nonEmpty, "\n\n")
}
