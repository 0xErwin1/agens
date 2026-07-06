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
// file shadows a built-in of the same name.
//
// A malformed or unreadable file is skipped and reported as a warning rather
// than failing the whole load: one bad hand-edited file must never take down the
// app or hide the built-ins and the other, valid agents. The returned []error
// lists those per-file (or per-directory) issues, and is nil when everything
// loaded cleanly. A missing directory contributes nothing and is not a warning.
func Load(globalDir, projectDir string) (*Set, []error) {
	set := newSet()

	for _, d := range Builtins() {
		set.put(d)
	}

	var warnings []error
	for _, dir := range []string{globalDir, projectDir} {
		if dir == "" {
			continue
		}
		warnings = append(warnings, loadDir(set, dir)...)
	}

	return set, warnings
}

// loadDir parses every definition file in dir into set, returning a warning for
// each file it skipped (unreadable, malformed frontmatter, or an invalid mode).
// Entries are read in the sorted order os.ReadDir guarantees, so discovery is
// deterministic. A missing directory yields no warnings; an unreadable directory
// yields a single warning.
func loadDir(set *Set, dir string) []error {
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return []error{fmt.Errorf("agentdef: skipped dir %s: %w", dir, err)}
	}

	var warnings []error
	for _, entry := range entries {
		if entry.IsDir() || filepath.Ext(entry.Name()) != definitionExt {
			continue
		}

		path := filepath.Join(dir, entry.Name())
		data, err := os.ReadFile(path)
		if err != nil {
			warnings = append(warnings, fmt.Errorf("agentdef: skipped %s: %w", path, err))
			continue
		}

		name := strings.TrimSuffix(entry.Name(), definitionExt)
		def, err := Parse(name, path, data)
		if err != nil {
			warnings = append(warnings, fmt.Errorf("agentdef: skipped %s: %w", path, err))
			continue
		}

		set.put(def)
	}

	return warnings
}
