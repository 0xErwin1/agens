package agentdef

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// definitionExt is the file extension a definition file must have to be
// discovered; the file stem (without it) becomes the agent name.
const definitionExt = ".md"

// Load builds a Set from the built-in definitions overlaid, in order, by the
// definition files in globalDir and then projectDir. Later sources override
// earlier ones by agent name, so a project file shadows a global file, and any
// file shadows a built-in of the same name. An empty directory path or a
// missing directory contributes nothing; only a real read or parse failure is
// returned as an error.
func Load(globalDir, projectDir string) (*Set, error) {
	set := newSet()

	for _, d := range Builtins() {
		set.put(d)
	}

	for _, dir := range []string{globalDir, projectDir} {
		if dir == "" {
			continue
		}
		if err := loadDir(set, dir); err != nil {
			return nil, err
		}
	}

	return set, nil
}

// loadDir parses every definition file in dir into set. Entries are read in the
// sorted order os.ReadDir guarantees, so discovery is deterministic. A missing
// directory is not an error; a malformed file is, so a typo surfaces at startup
// instead of silently dropping an agent.
func loadDir(set *Set, dir string) error {
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("agentdef: read dir %s: %w", dir, err)
	}

	for _, entry := range entries {
		if entry.IsDir() || filepath.Ext(entry.Name()) != definitionExt {
			continue
		}

		path := filepath.Join(dir, entry.Name())
		data, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("agentdef: read %s: %w", path, err)
		}

		name := strings.TrimSuffix(entry.Name(), definitionExt)
		def, err := Parse(name, path, data)
		if err != nil {
			return fmt.Errorf("agentdef: %s: %w", path, err)
		}

		set.put(def)
	}

	return nil
}
