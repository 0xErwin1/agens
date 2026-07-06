package agentdef

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"gopkg.in/yaml.v3"
)

// SaveModels persists def with its allowed-models set replaced by models,
// writing the definition back as a markdown file with YAML frontmatter. A
// definition that already lives inside projectDir is rewritten in place; a
// built-in or a definition sourced from outside the project (for example the
// global agents dir) is instead materialized into projectDir as <name>.md, so
// the edit stays scoped to this project and never mutates a shared global file.
// It returns the path written.
//
// The write is atomic (temp file plus rename), so a crash mid-write cannot leave
// a truncated file behind. Only the recognized frontmatter fields (description,
// mode, model, models) and the prompt body are preserved; any unrecognized
// frontmatter a hand-edited file carried is not round-tripped.
func SaveModels(projectDir string, def Definition, models []string) (string, error) {
	def.Models = cleanModels(models)

	path := targetPath(projectDir, def)
	if path == "" {
		return "", fmt.Errorf("agentdef: cannot save %q without a project directory", def.Name)
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return "", fmt.Errorf("agentdef: create dir for %s: %w", path, err)
	}

	data, err := marshal(def)
	if err != nil {
		return "", err
	}

	if err := writeFileAtomic(path, data); err != nil {
		return "", err
	}

	return path, nil
}

// targetPath decides where a models edit is written: in place when the
// definition already lives inside projectDir (or there is no project to scope
// to), otherwise a project-level <name>.md that shadows a built-in or a
// global-dir definition. It returns "" when there is nowhere to write (a
// non-file-backed definition and no project dir).
func targetPath(projectDir string, def Definition) string {
	fileBacked := def.Source != "" && def.Source != sourceBuiltin

	if fileBacked && (projectDir == "" || withinDir(projectDir, def.Source)) {
		return def.Source
	}
	if projectDir == "" {
		return ""
	}
	return filepath.Join(projectDir, def.Name+definitionExt)
}

// withinDir reports whether path resolves to a location inside dir.
func withinDir(dir, path string) bool {
	dirAbs, err1 := filepath.Abs(dir)
	pathAbs, err2 := filepath.Abs(path)
	if err1 != nil || err2 != nil {
		return false
	}

	rel, err := filepath.Rel(dirAbs, pathAbs)
	if err != nil {
		return false
	}
	return rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

// writeFileAtomic writes data to path via a temp file in the same directory
// followed by a rename, so a reader never observes a partially written file.
func writeFileAtomic(path string, data []byte) error {
	tmp, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".*")
	if err != nil {
		return fmt.Errorf("agentdef: create temp for %s: %w", path, err)
	}
	tmpName := tmp.Name()
	defer func() { _ = os.Remove(tmpName) }() // no-op after a successful rename

	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("agentdef: write temp for %s: %w", path, err)
	}
	if err := tmp.Chmod(0o644); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("agentdef: chmod temp for %s: %w", path, err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("agentdef: close temp for %s: %w", path, err)
	}

	if err := os.Rename(tmpName, path); err != nil {
		return fmt.Errorf("agentdef: replace %s: %w", path, err)
	}
	return nil
}

// marshal renders def as a frontmatter block followed by its prompt body. Empty
// frontmatter fields are omitted, so a definition with no restrictions produces
// a minimal file.
func marshal(def Definition) ([]byte, error) {
	fm := frontmatter{
		Description: def.Description,
		Model:       def.Model,
		Models:      def.Models,
	}
	if def.Mode != "" && def.Mode != ModeAll {
		fm.Mode = string(def.Mode)
	}

	var body bytes.Buffer
	encoder := yaml.NewEncoder(&body)
	encoder.SetIndent(2)
	if err := encoder.Encode(fm); err != nil {
		return nil, fmt.Errorf("agentdef: marshal frontmatter for %q: %w", def.Name, err)
	}
	if err := encoder.Close(); err != nil {
		return nil, fmt.Errorf("agentdef: marshal frontmatter for %q: %w", def.Name, err)
	}

	var out bytes.Buffer
	out.WriteString("---\n")
	out.Write(body.Bytes())
	out.WriteString("---\n")
	if def.Prompt != "" {
		out.WriteString("\n")
		out.WriteString(def.Prompt)
		out.WriteString("\n")
	}

	return out.Bytes(), nil
}
