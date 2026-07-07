package skill

import (
	"fmt"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/0xErwin1/agens/internal/frontmatter"
)

// The Anthropic Agent Skill contract caps the two required frontmatter fields.
// Enforcing the exact limits keeps a skill authored for Claude a drop-in here.
const (
	maxNameLen        = 64
	maxDescriptionLen = 1024
)

// skillFrontmatter mirrors the recognized SKILL.md frontmatter keys. Unknown
// keys are ignored so a skill can carry fields a newer version understands
// without failing to parse here.
type skillFrontmatter struct {
	Name        string `yaml:"name,omitempty"`
	Description string `yaml:"description,omitempty"`
}

// Parse turns a SKILL.md file's raw bytes into a Skill. dir is the skill's
// directory (the root for bundled-file reads) and source is the path to the
// SKILL.md, both kept for diagnostics. A leading `---` frontmatter block is
// required and must supply a name (<=64 chars) and a description (<=1024 chars);
// either being absent or over its limit is an error, matching the Anthropic
// contract so a skill authored for Claude loads unchanged.
func Parse(dir, source string, data []byte) (Skill, error) {
	front, body := frontmatter.Split(data)
	if front == nil {
		return Skill{}, fmt.Errorf("missing the required frontmatter block (name and description)")
	}

	var fm skillFrontmatter
	if err := yaml.Unmarshal(front, &fm); err != nil {
		return Skill{}, fmt.Errorf("invalid frontmatter: %w", err)
	}

	name := strings.TrimSpace(fm.Name)
	if name == "" {
		return Skill{}, fmt.Errorf("frontmatter is missing the required name field")
	}
	if len(name) > maxNameLen {
		return Skill{}, fmt.Errorf("name is %d chars, over the %d-char limit", len(name), maxNameLen)
	}

	description := strings.TrimSpace(fm.Description)
	if description == "" {
		return Skill{}, fmt.Errorf("frontmatter is missing the required description field")
	}
	if len(description) > maxDescriptionLen {
		return Skill{}, fmt.Errorf("description is %d chars, over the %d-char limit", len(description), maxDescriptionLen)
	}

	return Skill{
		Name:        name,
		Description: description,
		Body:        strings.TrimSpace(string(body)),
		Dir:         dir,
		Source:      source,
	}, nil
}
