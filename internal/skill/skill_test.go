package skill

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParse_FrontmatterAndBody(t *testing.T) {
	data := []byte("---\n" +
		"name: pdf-fill\n" +
		"description: Fill PDF forms. Use when given a form to complete.\n" +
		"---\n" +
		"Step 1. Read the form.\n")

	sk, err := Parse("/skills/pdf-fill", "/skills/pdf-fill/SKILL.md", data)
	if err != nil {
		t.Fatalf("Parse() error = %v, want nil", err)
	}

	if sk.Name != "pdf-fill" {
		t.Fatalf("Name = %q, want pdf-fill", sk.Name)
	}
	if !strings.HasPrefix(sk.Description, "Fill PDF forms") {
		t.Fatalf("Description = %q", sk.Description)
	}
	if sk.Body != "Step 1. Read the form." {
		t.Fatalf("Body = %q, want the trimmed markdown body", sk.Body)
	}
	if sk.Dir != "/skills/pdf-fill" {
		t.Fatalf("Dir = %q, want the skill directory", sk.Dir)
	}
}

func TestParse_MissingFrontmatterErrors(t *testing.T) {
	_, err := Parse("d", "d/SKILL.md", []byte("# Just markdown, no frontmatter\n"))
	if err == nil {
		t.Fatal("Parse() error = nil, want an error for a manifest with no frontmatter")
	}
}

func TestParse_MissingNameOrDescriptionErrors(t *testing.T) {
	if _, err := Parse("d", "d/SKILL.md", []byte("---\ndescription: has no name\n---\nbody\n")); err == nil {
		t.Fatal("Parse() error = nil, want an error when name is missing")
	}
	if _, err := Parse("d", "d/SKILL.md", []byte("---\nname: no-desc\n---\nbody\n")); err == nil {
		t.Fatal("Parse() error = nil, want an error when description is missing")
	}
}

func TestParse_EnforcesAnthropicLimits(t *testing.T) {
	longName := strings.Repeat("a", maxNameLen+1)
	if _, err := Parse("d", "d/SKILL.md", []byte("---\nname: "+longName+"\ndescription: ok\n---\nbody\n")); err == nil {
		t.Fatalf("Parse() error = nil, want an error for a name over %d chars", maxNameLen)
	}

	longDesc := strings.Repeat("d", maxDescriptionLen+1)
	if _, err := Parse("d", "d/SKILL.md", []byte("---\nname: ok\ndescription: "+longDesc+"\n---\nbody\n")); err == nil {
		t.Fatalf("Parse() error = nil, want an error for a description over %d chars", maxDescriptionLen)
	}
}

func TestParse_NameExactlyAtLimitIsAccepted(t *testing.T) {
	name := strings.Repeat("a", maxNameLen)
	sk, err := Parse("d", "d/SKILL.md", []byte("---\nname: "+name+"\ndescription: ok\n---\nbody\n"))
	if err != nil {
		t.Fatalf("Parse() error = %v, want the boundary length accepted", err)
	}
	if sk.Name != name {
		t.Fatalf("Name = %q, want the %d-char name", sk.Name, maxNameLen)
	}
}

func TestSet_ByNameAndOverride(t *testing.T) {
	set := newSet()
	set.put(Skill{Name: "a", Description: "first"})
	set.put(Skill{Name: "b", Description: "second"})
	set.put(Skill{Name: "a", Description: "overridden"})

	if set.Len() != 2 {
		t.Fatalf("Len = %d, want 2 (the override replaced, not appended)", set.Len())
	}
	got, ok := set.ByName("a")
	if !ok || got.Description != "overridden" {
		t.Fatalf("ByName(a) = %q, %v, want the overriding skill", got.Description, ok)
	}

	// The override kept a's original position ahead of b.
	all := set.All()
	if all[0].Name != "a" || all[1].Name != "b" {
		t.Fatalf("order = %v, want a before b preserved across the override", []string{all[0].Name, all[1].Name})
	}
}

func TestLoad_MissingDirectoriesAreNotAnError(t *testing.T) {
	set, warnings := Load(filepath.Join(t.TempDir(), "nope"), filepath.Join(t.TempDir(), "nada"))
	if len(warnings) != 0 {
		t.Fatalf("Load() warnings = %v, want none for missing directories", warnings)
	}
	if set.Len() != 0 {
		t.Fatalf("Load() = %d skills, want none", set.Len())
	}
}

func TestLoad_ProjectOverridesGlobal(t *testing.T) {
	globalDir := t.TempDir()
	projectDir := t.TempDir()

	writeSkill(t, globalDir, "shared", "---\nname: shared\ndescription: from global\n---\nglobal body\n")
	writeSkill(t, globalDir, "only-global", "---\nname: only-global\ndescription: g\n---\nbody\n")
	writeSkill(t, projectDir, "shared", "---\nname: shared\ndescription: from project\n---\nproject body\n")

	set, warnings := Load(globalDir, projectDir)
	if len(warnings) != 0 {
		t.Fatalf("Load() warnings = %v, want none", warnings)
	}

	shared, _ := set.ByName("shared")
	if shared.Description != "from project" {
		t.Fatalf("shared description = %q, want the project skill to override the global", shared.Description)
	}
	if _, ok := set.ByName("only-global"); !ok {
		t.Fatal("a global-only skill was dropped")
	}
}

func TestLoad_MalformedSkillIsSkippedWithWarning(t *testing.T) {
	dir := t.TempDir()
	writeSkill(t, dir, "broken", "---\ndescription: no name here\n---\nbody\n")
	writeSkill(t, dir, "good", "---\nname: good\ndescription: fine\n---\nok body\n")

	set, warnings := Load(dir, "")

	if len(warnings) != 1 {
		t.Fatalf("Load() warnings = %v, want exactly one for the malformed skill", warnings)
	}
	if !strings.Contains(warnings[0].Error(), "broken") {
		t.Fatalf("warning = %q, want it to name the skipped skill", warnings[0].Error())
	}
	if _, ok := set.ByName("good"); !ok {
		t.Fatal("the valid skill was dropped alongside the malformed one")
	}
}

func TestLoad_DirectoryWithoutManifestIsSkippedSilently(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "not-a-skill"), 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	writeSkill(t, dir, "real", "---\nname: real\ndescription: r\n---\nbody\n")

	set, warnings := Load(dir, "")
	if len(warnings) != 0 {
		t.Fatalf("Load() warnings = %v, want none for a plain subdirectory", warnings)
	}
	if set.Len() != 1 {
		t.Fatalf("Load() = %d skills, want only the one real skill", set.Len())
	}
}

// writeSkill creates <dir>/<name>/SKILL.md with the given content.
func writeSkill(t *testing.T, dir, name, content string) {
	t.Helper()
	skillDir := filepath.Join(dir, name)
	if err := os.MkdirAll(skillDir, 0o755); err != nil {
		t.Fatalf("mkdir skill dir %s: %v", skillDir, err)
	}
	if err := os.WriteFile(filepath.Join(skillDir, manifestName), []byte(content), 0o644); err != nil {
		t.Fatalf("write manifest for %s: %v", name, err)
	}
}
