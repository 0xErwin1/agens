package prompt

import (
	"strings"
	"testing"
	"time"
)

func TestSkillsSection_EmptyIsDropped(t *testing.T) {
	if got := skillsSection(nil); got != "" {
		t.Fatalf("skillsSection(nil) = %q, want empty", got)
	}
}

func TestSkillsSection_ListsNameAndDescription(t *testing.T) {
	got := skillsSection([]SkillInfo{
		{Name: "git-commit", Description: "make conventional commits"},
		{Name: "pdf-fill", Description: "fill PDF forms"},
	})

	if !strings.Contains(got, "git-commit") || !strings.Contains(got, "make conventional commits") {
		t.Fatalf("section = %q, want the first skill's name and description", got)
	}
	if !strings.Contains(got, "pdf-fill") || !strings.Contains(got, "fill PDF forms") {
		t.Fatalf("section = %q, want the second skill listed", got)
	}
	if !strings.Contains(got, "`skill` tool") {
		t.Fatalf("section = %q, want the instruction to load a skill via the skill tool", got)
	}
}

func TestBuild_AppendsSkillsSectionWhenPresent(t *testing.T) {
	out := Build(Options{
		Override: "BASE PROMPT",
		Model:    "gpt-5.5",
		Now:      time.Unix(0, 0).UTC(),
		Skills:   []SkillInfo{{Name: "git-commit", Description: "make commits"}},
	})

	if !strings.Contains(out, "BASE PROMPT") {
		t.Fatalf("prompt = %q, want the base kept", out)
	}
	if !strings.Contains(out, "Available skills") || !strings.Contains(out, "git-commit") {
		t.Fatalf("prompt = %q, want the skills section appended", out)
	}
}

func TestBuild_NoSkillsSectionWhenNone(t *testing.T) {
	out := Build(Options{
		Override: "BASE PROMPT",
		Model:    "gpt-5.5",
		Now:      time.Unix(0, 0).UTC(),
	})

	if strings.Contains(out, "Available skills") {
		t.Fatalf("prompt = %q, want no skills section when none are configured", out)
	}
}
