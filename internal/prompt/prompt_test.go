package prompt

import (
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestBuildUsesSelectWhenNoOverride(t *testing.T) {
	configHome := t.TempDir()
	projectRoot := t.TempDir()

	got := Build(Options{
		Model:       "gpt-5-codex",
		WorkingDir:  projectRoot,
		ProjectRoot: projectRoot,
		ConfigHome:  configHome,
		IsGitRepo:   true,
		Platform:    "linux",
		Now:         time.Date(2026, 7, 4, 0, 0, 0, 0, time.UTC),
	})

	if !strings.Contains(got, "running on a reasoning model") {
		t.Fatalf("Build() = %q, want codex base persona", got)
	}
	if !strings.Contains(got, "You are powered by the model named gpt-5-codex.") {
		t.Fatalf("Build() = %q, want environment block", got)
	}
}

func TestBuildOverrideReplacesBaseButKeepsEnvAndInstructions(t *testing.T) {
	configHome := t.TempDir()
	projectRoot := t.TempDir()
	writeFile(t, filepath.Join(projectRoot, "AGENTS.md"), "project rules")

	got := Build(Options{
		Model:       "gpt-5.5",
		Override:    "Custom persona text.",
		WorkingDir:  projectRoot,
		ProjectRoot: projectRoot,
		ConfigHome:  configHome,
		IsGitRepo:   false,
		Platform:    "linux",
		Now:         time.Date(2026, 7, 4, 0, 0, 0, 0, time.UTC),
	})

	if !strings.Contains(got, "Custom persona text.") {
		t.Fatalf("Build() = %q, want override persona", got)
	}
	if strings.Contains(got, "interactive CLI coding agent") {
		t.Fatalf("Build() = %q, want default base persona replaced", got)
	}
	if !strings.Contains(got, "You are powered by the model named gpt-5.5.") {
		t.Fatalf("Build() = %q, want environment block kept", got)
	}
	if !strings.Contains(got, "project rules") {
		t.Fatalf("Build() = %q, want instructions kept", got)
	}
}

func TestBuildJoinsPartsWithBlankLines(t *testing.T) {
	configHome := t.TempDir()
	projectRoot := t.TempDir()
	writeFile(t, filepath.Join(projectRoot, "AGENTS.md"), "project rules")

	got := Build(Options{
		Model:       "gpt-5.5",
		WorkingDir:  projectRoot,
		ProjectRoot: projectRoot,
		ConfigHome:  configHome,
		IsGitRepo:   true,
		Platform:    "linux",
		Now:         time.Date(2026, 7, 4, 0, 0, 0, 0, time.UTC),
	})

	parts := strings.Split(got, "\n\n")
	if len(parts) < 3 {
		t.Fatalf("Build() has %d blank-line-separated parts, want at least 3: %q", len(parts), got)
	}
}

func TestJoinNonEmptyDropsEmptyPartsAndReturnsEmptyWhenAllPartsAreEmpty(t *testing.T) {
	if got := joinNonEmpty([]string{"", "", ""}); got != "" {
		t.Fatalf("joinNonEmpty(all empty) = %q, want empty string", got)
	}

	got := joinNonEmpty([]string{"a", "", "b"})
	want := "a\n\nb"
	if got != want {
		t.Fatalf("joinNonEmpty() = %q, want %q", got, want)
	}
}
