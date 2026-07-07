package agentdef

import (
	"fmt"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/0xErwin1/agens/internal/frontmatter"
)

// defFrontmatter mirrors the recognized YAML keys of a definition's frontmatter
// block. Unknown keys are ignored so a file can carry fields a newer version
// understands without failing to parse here.
type defFrontmatter struct {
	Description string   `yaml:"description,omitempty"`
	Mode        string   `yaml:"mode,omitempty"`
	Model       string   `yaml:"model,omitempty"`
	Models      []string `yaml:"models,omitempty"`
}

// Parse turns a definition file's raw bytes into a Definition. name is the file
// stem (the agent name) and source is the path, kept for diagnostics. A leading
// `---` YAML frontmatter block is optional: when absent, the whole file is the
// prompt and every field takes its default (ModeAll, no model restriction).
func Parse(name, source string, data []byte) (Definition, error) {
	front, body := frontmatter.Split(data)

	def := Definition{
		Name:   name,
		Source: source,
		Mode:   ModeAll,
		Prompt: strings.TrimSpace(string(body)),
	}

	if front == nil {
		return def, nil
	}

	var fm defFrontmatter
	if err := yaml.Unmarshal(front, &fm); err != nil {
		return Definition{}, fmt.Errorf("invalid frontmatter: %w", err)
	}

	mode, err := parseMode(fm.Mode)
	if err != nil {
		return Definition{}, err
	}

	def.Description = strings.TrimSpace(fm.Description)
	def.Mode = mode
	def.Model = strings.TrimSpace(fm.Model)
	def.Models = cleanModels(fm.Models)

	return def, nil
}

// parseMode validates the frontmatter mode field. An empty value defaults to
// ModeAll; any value other than the three known modes is an error.
func parseMode(s string) (Mode, error) {
	switch strings.TrimSpace(s) {
	case "", string(ModeAll):
		return ModeAll, nil
	case string(ModePrimary):
		return ModePrimary, nil
	case string(ModeSubagent):
		return ModeSubagent, nil
	default:
		return "", fmt.Errorf("invalid mode %q (want primary, subagent, or all)", s)
	}
}
