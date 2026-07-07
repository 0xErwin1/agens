package fs

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/0xErwin1/agens/internal/tool"
)

// Read implements the "read" tool: it returns the contents of a file
// confined to dir, optionally sliced to a 1-based line range.
type Read struct {
	dir *Dir
}

// NewRead returns a Read tool rooted at dir. It panics if dir is nil, since
// a nil dir is a wiring bug the composition root must fail fast on.
func NewRead(dir *Dir) *Read {
	if dir == nil {
		panic("fs: NewRead called with a nil Dir")
	}
	return &Read{dir: dir}
}

func (r *Read) Name() string { return "read" }

func (r *Read) Description() string {
	return "Read the contents of a file, optionally limited to a range of lines."
}

func (r *Read) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"path":   {Type: "string", Description: "path to the file, relative to the project root"},
			"offset": {Type: "integer", Description: "1-based line number to start reading from (default: 1)"},
			"limit":  {Type: "integer", Description: "maximum number of lines to return (default: all)"},
		},
		Required: []string{"path"},
	}
}

// readInput is the schema of Read's Execute input.
type readInput struct {
	Path   string `json:"path"`
	Offset int    `json:"offset"`
	Limit  int    `json:"limit"`
}

func (r *Read) Execute(_ context.Context, input json.RawMessage) (tool.Result, error) {
	var in readInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("read: invalid input: %v", err)}, nil
	}
	if in.Path == "" {
		return tool.Result{IsError: true, Text: "read: invalid input: path is required"}, nil
	}
	if in.Offset < 0 || in.Limit < 0 {
		return tool.Result{IsError: true, Text: "read: offset and limit must not be negative"}, nil
	}

	relPath, err := r.dir.rel(in.Path)
	if err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	data, err := r.dir.root.ReadFile(relPath)
	if err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	if in.Offset <= 1 && in.Limit <= 0 {
		return tool.Result{Text: string(data)}, nil
	}
	return sliceLines(string(data), in.Offset, in.Limit)
}

// sliceLines returns the lines of content in the 1-based range
// [offset, offset+limit), where an offset of 0 defaults to 1 and a limit of
// 0 means "to the end of the file".
func sliceLines(content string, offset, limit int) (tool.Result, error) {
	lines := splitLines(content)

	start := offset
	if start == 0 {
		start = 1
	}
	if start > len(lines) {
		return tool.Result{
			IsError: true,
			Text:    fmt.Sprintf("read: offset %d is past the end of the file (%d lines)", start, len(lines)),
		}, nil
	}

	end := len(lines)
	if limit > 0 && start-1+limit < end {
		end = start - 1 + limit
	}

	return tool.Result{Text: strings.Join(lines[start-1:end], "\n")}, nil
}
