package search

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"

	"github.com/bmatcuk/doublestar/v4"
	"github.com/google/jsonschema-go/jsonschema"
	"github.com/0xErwin1/agens/internal/tool"
)

// Glob implements the "glob" tool: it lists worktree-relative file paths
// matching a doublestar pattern, confined to fsys.
type Glob struct {
	fsys fs.FS
}

// NewGlob returns a Glob tool operating over fsys. It panics if fsys is nil,
// since a nil fsys is a wiring bug the composition root must fail fast on.
func NewGlob(fsys fs.FS) *Glob {
	if fsys == nil {
		panic("search: NewGlob called with a nil fs.FS")
	}
	return &Glob{fsys: fsys}
}

func (g *Glob) Name() string { return "glob" }

func (g *Glob) Description() string {
	return "List file paths matching a glob pattern (** matches nested directories), " +
		"confined to the project root."
}

func (g *Glob) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"pattern": {
				Type: "string",
				Description: `glob pattern to match file paths against, relative to the project root, ` +
					`e.g. "**/*.go" (** matches nested directories)`,
			},
		},
		Required: []string{"pattern"},
	}
}

// globInput is the schema of Glob's Execute input.
type globInput struct {
	Pattern string `json:"pattern"`
}

func (g *Glob) Execute(_ context.Context, input json.RawMessage) (tool.Result, error) {
	var in globInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("glob: invalid input: %v", err)}, nil
	}
	if in.Pattern == "" {
		return tool.Result{IsError: true, Text: "glob: invalid input: pattern is required"}, nil
	}
	if err := validateRel("glob", "pattern", in.Pattern); err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	out := newCapped(maxItems, maxOutputBytes, "paths")
	walkErr := doublestar.GlobWalk(g.fsys, in.Pattern, func(p string, _ fs.DirEntry) error {
		if !out.add(p) {
			return errCapReached
		}
		return nil
	}, doublestar.WithFilesOnly(), doublestar.WithNoHidden())

	if walkErr != nil && !errors.Is(walkErr, errCapReached) {
		if errors.Is(walkErr, doublestar.ErrBadPattern) {
			return tool.Result{IsError: true, Text: fmt.Sprintf("glob: invalid pattern: %v", walkErr)}, nil
		}
		return tool.Result{IsError: true, Text: fmt.Sprintf("glob: %v", walkErr)}, nil
	}

	return finishWalk(out, walkErr), nil
}
