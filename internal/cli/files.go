package cli

import (
	"errors"
	iofs "io/fs"

	agentfs "github.com/iperez/agens/internal/tool/fs"
	"github.com/iperez/agens/internal/tui"
)

// maxProjectFiles bounds how many files the @-reference index holds, so a huge
// repository cannot build an unbounded list.
const maxProjectFiles = 5000

// projectFileSource lists and reads the project's files for the TUI's
// @-references, confined to the project root by the same os.Root the tools use.
type projectFileSource struct {
	dir  *agentfs.Dir
	fsys iofs.FS
}

var _ tui.FileSource = (*projectFileSource)(nil)

// newProjectFileSource opens root as a confinement root for @-references.
func newProjectFileSource(root string) (*projectFileSource, error) {
	dir, err := agentfs.Open(root)
	if err != nil {
		return nil, err
	}
	return &projectFileSource{dir: dir, fsys: dir.FS()}, nil
}

// List walks the project tree and returns the file paths, skipping common
// heavy or noise directories and capping the total.
func (p *projectFileSource) List() ([]string, error) {
	var files []string

	err := iofs.WalkDir(p.fsys, ".", func(path string, d iofs.DirEntry, err error) error {
		if err != nil {
			if d != nil && d.IsDir() {
				return iofs.SkipDir
			}
			return nil
		}

		if d.IsDir() {
			if path != "." && skipDir(d.Name()) {
				return iofs.SkipDir
			}
			return nil
		}

		files = append(files, path)
		if len(files) >= maxProjectFiles {
			return iofs.SkipAll
		}
		return nil
	})
	if err != nil && !errors.Is(err, iofs.SkipAll) {
		return files, err
	}
	return files, nil
}

// Read returns the contents of a project file.
func (p *projectFileSource) Read(path string) (string, error) {
	data, err := iofs.ReadFile(p.fsys, path)
	if err != nil {
		return "", err
	}
	return string(data), nil
}

// skipDir reports whether a directory should be excluded from the file index.
func skipDir(name string) bool {
	switch name {
	case ".git", "node_modules", "vendor", ".direnv", "dist", "build", "target",
		".next", ".venv", "__pycache__", ".idea", ".vscode", ".terraform":
		return true
	default:
		return false
	}
}
