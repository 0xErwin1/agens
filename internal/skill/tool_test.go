package skill

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// toolWithSkill builds a one-skill set rooted at a real temp directory holding a
// SKILL.md and any extra bundled files, then returns the skill tool over it.
func toolWithSkill(t *testing.T, body string, files map[string]string) (*Tool, string) {
	t.Helper()
	dir := t.TempDir()

	if err := os.WriteFile(filepath.Join(dir, manifestName), []byte(body), 0o644); err != nil {
		t.Fatalf("write manifest: %v", err)
	}
	for name, content := range files {
		full := filepath.Join(dir, name)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatalf("mkdir for %s: %v", name, err)
		}
		if err := os.WriteFile(full, []byte(content), 0o644); err != nil {
			t.Fatalf("write bundled file %s: %v", name, err)
		}
	}

	sk, err := Parse(dir, filepath.Join(dir, manifestName), []byte(body))
	if err != nil {
		t.Fatalf("parse skill: %v", err)
	}
	set := newSet()
	set.put(sk)

	return NewTool(set), dir
}

func run(t *testing.T, tl *Tool, in skillInput) (text string, isErr bool) {
	t.Helper()
	raw, err := json.Marshal(in)
	if err != nil {
		t.Fatalf("marshal input: %v", err)
	}
	res, err := tl.Execute(context.Background(), raw)
	if err != nil {
		t.Fatalf("Execute() returned a Go error = %v, want a tool result", err)
	}
	return res.Text, res.IsError
}

func TestTool_SchemaAdvertisesSkillNamesAsEnum(t *testing.T) {
	tl, _ := toolWithSkill(t, "---\nname: git-commit\ndescription: make commits\n---\nbody\n", nil)

	schema := tl.Schema()
	name := schema.Properties["name"]
	if len(name.Enum) != 1 || name.Enum[0] != "git-commit" {
		t.Fatalf("name enum = %v, want [git-commit]", name.Enum)
	}
	if len(schema.Required) != 1 || schema.Required[0] != "name" {
		t.Fatalf("required = %v, want [name]", schema.Required)
	}
}

func TestTool_LoadsManifestBodyAtLevelTwo(t *testing.T) {
	tl, dir := toolWithSkill(t, "---\nname: git-commit\ndescription: make commits\n---\nStep one, stage.\n", nil)

	text, isErr := run(t, tl, skillInput{Name: "git-commit"})
	if isErr {
		t.Fatalf("level-2 load errored: %q", text)
	}
	if !strings.Contains(text, "Step one, stage.") {
		t.Fatalf("output = %q, want the SKILL.md body", text)
	}
	if !strings.Contains(text, dir) {
		t.Fatalf("output = %q, want the skill directory named so the model can resolve bundled paths", text)
	}
}

func TestTool_ReadsBundledFileAtLevelThree(t *testing.T) {
	tl, _ := toolWithSkill(t,
		"---\nname: git-commit\ndescription: make commits\n---\nbody\n",
		map[string]string{"references/style.md": "Use conventional commits."},
	)

	text, isErr := run(t, tl, skillInput{Name: "git-commit", Path: "references/style.md"})
	if isErr {
		t.Fatalf("level-3 read errored: %q", text)
	}
	if text != "Use conventional commits." {
		t.Fatalf("output = %q, want the bundled file content", text)
	}
}

func TestTool_UnknownSkillIsAToolError(t *testing.T) {
	tl, _ := toolWithSkill(t, "---\nname: git-commit\ndescription: d\n---\nbody\n", nil)

	text, isErr := run(t, tl, skillInput{Name: "nope"})
	if !isErr {
		t.Fatalf("output = %q, want a tool error for an unknown skill", text)
	}
	if !strings.Contains(text, "git-commit") {
		t.Fatalf("output = %q, want the available skills listed", text)
	}
}

func TestTool_PathEscapeIsRejected(t *testing.T) {
	tl, dir := toolWithSkill(t, "---\nname: git-commit\ndescription: d\n---\nbody\n", nil)

	// A secret one level above the skill directory must not be reachable.
	secret := filepath.Join(filepath.Dir(dir), "secret.txt")
	if err := os.WriteFile(secret, []byte("top secret"), 0o644); err != nil {
		t.Fatalf("write secret: %v", err)
	}

	text, isErr := run(t, tl, skillInput{Name: "git-commit", Path: "../secret.txt"})
	if !isErr {
		t.Fatalf("output = %q, want a tool error for a path escaping the skill directory", text)
	}
	if strings.Contains(text, "top secret") {
		t.Fatal("the traversal read a file outside the skill directory")
	}
}

func TestTool_TruncatesOversizedBundledFile(t *testing.T) {
	big := strings.Repeat("x", maxFileBytes+100)
	tl, _ := toolWithSkill(t,
		"---\nname: git-commit\ndescription: d\n---\nbody\n",
		map[string]string{"big.txt": big},
	)

	text, isErr := run(t, tl, skillInput{Name: "git-commit", Path: "big.txt"})
	if isErr {
		t.Fatalf("oversized read errored: %q", text)
	}
	if !strings.Contains(text, "truncated") {
		t.Fatal("output was not marked truncated")
	}
	if len(text) > maxFileBytes+200 {
		t.Fatalf("output length = %d, want it capped near the read limit", len(text))
	}
}
