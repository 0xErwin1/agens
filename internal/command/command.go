// Package command discovers user-authored slash commands from markdown files.
package command

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/0xErwin1/agens/internal/frontmatter"
)

// Command is one discovered user slash command.
type Command struct {
	Name         string
	Description  string
	ArgumentHint string
	Body         string
	Source       string
}

// Set is an ordered, name-indexed collection of commands. Later puts replace an
// existing command without moving it, so project commands shadow global ones in
// stable discovery order.
type Set struct {
	commands []Command
	index    map[string]int
}

type commandFrontmatter struct {
	Description  string `yaml:"description,omitempty"`
	ArgumentHint string `yaml:"argument-hint,omitempty"`
}

// NewSet builds a command set from an existing slice.
func NewSet(commands []Command) *Set {
	set := newSet()
	for _, cmd := range commands {
		set.put(cmd)
	}
	return set
}

func newSet() *Set {
	return &Set{index: make(map[string]int)}
}

func (s *Set) put(cmd Command) {
	if i, ok := s.index[cmd.Name]; ok {
		s.commands[i] = cmd
		return
	}
	s.index[cmd.Name] = len(s.commands)
	s.commands = append(s.commands, cmd)
}

// All returns every command in discovery order.
func (s *Set) All() []Command {
	out := make([]Command, len(s.commands))
	copy(out, s.commands)
	return out
}

// Len reports how many commands the set holds.
func (s *Set) Len() int { return len(s.commands) }

// Parse turns a markdown command file into a Command. name comes from the file
// stem; optional frontmatter may provide description and argument-hint.
func Parse(name, source string, data []byte) (Command, error) {
	name = strings.TrimSpace(name)
	if name == "" {
		return Command{}, fmt.Errorf("empty command name")
	}
	if strings.Contains(name, "/") {
		return Command{}, fmt.Errorf("command name %q contains /", name)
	}

	front, body := frontmatter.Split(data)
	var fm commandFrontmatter
	if front != nil {
		if err := yaml.Unmarshal(front, &fm); err != nil {
			return Command{}, fmt.Errorf("invalid frontmatter: %w", err)
		}
	}

	return Command{
		Name:         name,
		Description:  strings.TrimSpace(fm.Description),
		ArgumentHint: strings.TrimSpace(fm.ArgumentHint),
		Body:         strings.TrimSpace(string(body)),
		Source:       source,
	}, nil
}

// Load discovers direct *.md command files in globalDir and then projectDir. A
// project file with the same stem overrides a global command without reordering.
func Load(globalDir, projectDir string) (*Set, []error) {
	set := newSet()
	var warnings []error
	for _, dir := range []string{globalDir, projectDir} {
		if dir == "" {
			continue
		}
		warnings = append(warnings, loadDir(set, dir)...)
	}
	return set, warnings
}

func loadDir(set *Set, dir string) []error {
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return []error{fmt.Errorf("command: skipped dir %s: %w", dir, err)}
	}

	var warnings []error
	for _, entry := range entries {
		if entry.IsDir() || filepath.Ext(entry.Name()) != ".md" {
			continue
		}

		path := filepath.Join(dir, entry.Name())
		data, err := os.ReadFile(path)
		if err != nil {
			warnings = append(warnings, fmt.Errorf("command: skipped %s: %w", path, err))
			continue
		}

		cmd, err := Parse(strings.TrimSuffix(entry.Name(), filepath.Ext(entry.Name())), path, data)
		if err != nil {
			warnings = append(warnings, fmt.Errorf("command: skipped %s: %w", path, err))
			continue
		}
		set.put(cmd)
	}
	return warnings
}

// Expand renders the command body with the user's trailing argument text.
func (c Command) Expand(arguments string) string {
	return strings.ReplaceAll(c.Body, "$ARGUMENTS", strings.TrimSpace(arguments))
}
