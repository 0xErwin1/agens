package prompt

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestInstructionsIncludesGlobalFile(t *testing.T) {
	configHome := t.TempDir()
	projectRoot := t.TempDir()
	writeFile(t, filepath.Join(configHome, "AGENTS.md"), "global guidance")

	got := Instructions(configHome, projectRoot, projectRoot)

	if len(got) != 1 {
		t.Fatalf("Instructions() = %v, want 1 entry", got)
	}
	want := "Instructions from: " + filepath.Join(configHome, "AGENTS.md") + "\nglobal guidance"
	if got[0] != want {
		t.Fatalf("Instructions()[0] = %q, want %q", got[0], want)
	}
}

func TestInstructionsProjectWalkUpPrefersAgentsOverClaudeOverContext(t *testing.T) {
	projectRoot := t.TempDir()
	writeFile(t, filepath.Join(projectRoot, "AGENTS.md"), "agents content")
	writeFile(t, filepath.Join(projectRoot, "CLAUDE.md"), "claude content")
	writeFile(t, filepath.Join(projectRoot, "CONTEXT.md"), "context content")

	got := Instructions(t.TempDir(), projectRoot, projectRoot)

	if len(got) != 1 {
		t.Fatalf("Instructions() = %v, want 1 entry", got)
	}
	if !strings.Contains(got[0], "agents content") {
		t.Fatalf("Instructions()[0] = %q, want AGENTS.md content", got[0])
	}
}

func TestInstructionsNestedWorkingDirFindsParentAgentsFile(t *testing.T) {
	projectRoot := t.TempDir()
	nested := filepath.Join(projectRoot, "a", "b", "c")
	if err := os.MkdirAll(nested, 0o755); err != nil {
		t.Fatal(err)
	}
	writeFile(t, filepath.Join(projectRoot, "AGENTS.md"), "root guidance")

	got := Instructions(t.TempDir(), nested, projectRoot)

	if len(got) != 1 {
		t.Fatalf("Instructions() = %v, want 1 entry", got)
	}
	if !strings.Contains(got[0], "root guidance") {
		t.Fatalf("Instructions()[0] = %q, want root AGENTS.md content", got[0])
	}
}

func TestInstructionsFirstFilenameWinsEvenWhenFoundHigherInTree(t *testing.T) {
	projectRoot := t.TempDir()
	nested := filepath.Join(projectRoot, "nested")
	if err := os.MkdirAll(nested, 0o755); err != nil {
		t.Fatal(err)
	}
	writeFile(t, filepath.Join(projectRoot, "AGENTS.md"), "root agents")
	writeFile(t, filepath.Join(nested, "CLAUDE.md"), "nested claude")

	got := Instructions(t.TempDir(), nested, projectRoot)

	if len(got) != 1 {
		t.Fatalf("Instructions() = %v, want 1 entry", got)
	}
	if !strings.Contains(got[0], "root agents") {
		t.Fatalf("Instructions()[0] = %q, want root AGENTS.md to win over nested CLAUDE.md", got[0])
	}
}

func TestInstructionsReturnsEmptySliceWhenNothingPresent(t *testing.T) {
	got := Instructions(t.TempDir(), t.TempDir(), t.TempDir())

	if len(got) != 0 {
		t.Fatalf("Instructions() = %v, want empty", got)
	}
}

func TestInstructionsTruncatesOversizedFile(t *testing.T) {
	configHome := t.TempDir()
	projectRoot := t.TempDir()
	oversized := strings.Repeat("x", maxInstructionBytes+1024)
	writeFile(t, filepath.Join(configHome, "AGENTS.md"), oversized)

	got := Instructions(configHome, projectRoot, projectRoot)

	if len(got) != 1 {
		t.Fatalf("Instructions() = %v, want 1 entry", got)
	}
	if len(got[0]) > maxInstructionBytes+len("Instructions from: \n")+len(filepath.Join(configHome, "AGENTS.md")) {
		t.Fatalf("Instructions()[0] length = %d, want capped content", len(got[0]))
	}
}
