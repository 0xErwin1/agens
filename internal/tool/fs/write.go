package fs

import (
	"context"
	"encoding/json"
	"fmt"
	"path/filepath"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/0xErwin1/agens/internal/tool"
)

// Write implements the "write" tool: it creates or overwrites a file
// confined to dir, auto-creating any missing parent directories.
type Write struct {
	dir *Dir
}

// NewWrite returns a Write tool rooted at dir. It panics if dir is nil,
// since a nil dir is a wiring bug the composition root must fail fast on.
func NewWrite(dir *Dir) *Write {
	if dir == nil {
		panic("fs: NewWrite called with a nil Dir")
	}
	return &Write{dir: dir}
}

func (w *Write) Name() string { return "write" }

func (w *Write) Description() string {
	return "Create or overwrite a file, creating any missing parent directories."
}

func (w *Write) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"path":    {Type: "string", Description: "path to the file, relative to the project root"},
			"content": {Type: "string", Description: "the full content to write"},
		},
		Required: []string{"path", "content"},
	}
}

// writeInput is the schema of Write's Execute input. Content is a pointer so
// an absent field is distinguishable from an explicit empty string: omitting
// content is a caller error, not a request to truncate the file to empty.
type writeInput struct {
	Path    string  `json:"path"`
	Content *string `json:"content"`
}

func (w *Write) Execute(_ context.Context, input json.RawMessage) (tool.Result, error) {
	var in writeInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("write: invalid input: %v", err)}, nil
	}
	if in.Path == "" {
		return tool.Result{IsError: true, Text: "write: invalid input: path is required"}, nil
	}
	if in.Content == nil {
		return tool.Result{IsError: true, Text: "write: invalid input: content is required"}, nil
	}

	relPath, err := w.dir.rel(in.Path)
	if err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	// Read the current content before overwriting so the result can show the
	// change as a diff. A missing (or unreadable) file is treated as empty, so
	// a brand-new file renders as all additions.
	before := ""
	if data, err := w.dir.root.ReadFile(relPath); err == nil {
		before = string(data)
	}

	if parent := filepath.Dir(relPath); parent != "." {
		if err := w.dir.root.MkdirAll(parent, 0o755); err != nil {
			return tool.Result{IsError: true, Text: err.Error()}, nil
		}
	}

	if err := w.dir.root.WriteFile(relPath, []byte(*in.Content), 0o644); err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	return tool.Result{Text: renderDiff(in.Path, before, *in.Content)}, nil
}
