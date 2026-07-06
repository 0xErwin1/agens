package skill

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

// manifestName is the file each skill directory must contain to be discovered.
const manifestName = "SKILL.md"

// Load discovers skills in globalDir and then projectDir. Each directory holds
// one subdirectory per skill, and each skill subdirectory must contain a
// SKILL.md. A project skill overrides a global one of the same name.
//
// A malformed or unreadable SKILL.md is skipped and reported as a warning rather
// than failing the whole load: one bad hand-edited skill must never take down the
// app or hide the others. The returned []error lists those per-file (or
// per-directory) issues, and is nil when everything loaded cleanly. A
// subdirectory without a SKILL.md is not a skill and is skipped silently; a
// missing top-level directory contributes nothing and is not a warning.
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

// loadDir parses the SKILL.md of every skill subdirectory in dir into set,
// returning a warning for each skill it skipped (unreadable or malformed
// manifest). Subdirectories are visited in the sorted order os.ReadDir
// guarantees, so discovery is deterministic. A missing dir yields no warnings;
// an unreadable dir yields a single warning.
func loadDir(set *Set, dir string) []error {
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return []error{fmt.Errorf("skill: skipped dir %s: %w", dir, err)}
	}

	var warnings []error
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}

		skillDir := filepath.Join(dir, entry.Name())
		manifest := filepath.Join(skillDir, manifestName)

		data, err := os.ReadFile(manifest)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			warnings = append(warnings, fmt.Errorf("skill: skipped %s: %w", manifest, err))
			continue
		}

		sk, err := Parse(skillDir, manifest, data)
		if err != nil {
			warnings = append(warnings, fmt.Errorf("skill: skipped %s: %w", manifest, err))
			continue
		}

		set.put(sk)
	}

	return warnings
}
