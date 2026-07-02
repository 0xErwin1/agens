// Package fs provides the shared filesystem confinement boundary used by
// the read, write, and edit tools. All file access is routed through an
// os.Root opened on a single directory, so path escapes (parent traversal,
// absolute paths outside the root, and symlinks resolving outside the root)
// are rejected by the operating system itself rather than by hand-rolled
// prefix checks.
package fs

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

// Dir confines filesystem access to a single directory tree via os.Root.
// It is safe to share across the read, write, and edit tools: os.Root's
// methods are safe for concurrent use.
type Dir struct {
	path string
	root *os.Root
}

// Open opens dir as a confinement root. The returned Dir is held for the
// process lifetime; there is no Close in v1 since the composition root that
// creates it lives as long as the agent loop itself.
func Open(dir string) (*Dir, error) {
	abs, err := filepath.Abs(dir)
	if err != nil {
		return nil, fmt.Errorf("fs: resolve %q: %w", dir, err)
	}

	root, err := os.OpenRoot(abs)
	if err != nil {
		return nil, fmt.Errorf("fs: open root %q: %w", abs, err)
	}

	return &Dir{path: abs, root: root}, nil
}

// rel resolves p to a name suitable for passing to a *os.Root method. An
// absolute p is made relative to the root's path; a relative p is passed
// through cleaned but otherwise unchecked, since os.Root itself rejects any
// name whose resolution would escape the root.
func (d *Dir) rel(p string) (string, error) {
	if p == "" {
		return "", errors.New("fs: empty path")
	}

	if filepath.IsAbs(p) {
		r, err := filepath.Rel(d.path, p)
		if err != nil {
			return "", fmt.Errorf("fs: resolve %q relative to %q: %w", p, d.path, err)
		}
		return r, nil
	}

	return filepath.Clean(p), nil
}
