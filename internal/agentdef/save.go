package agentdef

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"

	"gopkg.in/yaml.v3"
)

// SaveModels persists def with its allowed-models set replaced by models,
// writing the definition back as a markdown file with YAML frontmatter. A
// file-backed definition is rewritten in place; a built-in (which has no file)
// is materialized into projectDir as <name>.md, so editing a built-in's models
// creates a project-level definition that shadows it. It returns the path
// written.
//
// Only the recognized frontmatter fields (description, mode, model, models) and
// the prompt body are preserved; any unrecognized frontmatter a hand-edited file
// carried is not round-tripped.
func SaveModels(projectDir string, def Definition, models []string) (string, error) {
	def.Models = cleanModels(models)

	path := def.Source
	if path == "" || def.Source == sourceBuiltin {
		if projectDir == "" {
			return "", fmt.Errorf("agentdef: cannot save built-in %q without a project directory", def.Name)
		}
		path = filepath.Join(projectDir, def.Name+definitionExt)
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return "", fmt.Errorf("agentdef: create dir for %s: %w", path, err)
	}

	data, err := marshal(def)
	if err != nil {
		return "", err
	}

	if err := os.WriteFile(path, data, 0o644); err != nil {
		return "", fmt.Errorf("agentdef: write %s: %w", path, err)
	}

	return path, nil
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
