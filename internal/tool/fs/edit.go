package fs

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/iperez/agens/internal/tool"
)

// Edit implements the "edit" tool: it replaces a single, unambiguous
// occurrence of old_string with new_string in a file confined to dir.
type Edit struct {
	dir *Dir
}

// NewEdit returns an Edit tool rooted at dir. It panics if dir is nil,
// since a nil dir is a wiring bug the composition root must fail fast on.
func NewEdit(dir *Dir) *Edit {
	if dir == nil {
		panic("fs: NewEdit called with a nil Dir")
	}
	return &Edit{dir: dir}
}

func (e *Edit) Name() string { return "edit" }

func (e *Edit) Description() string {
	return "Replace a single, unambiguous occurrence of old_string with new_string in a file."
}

func (e *Edit) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"path":       {Type: "string", Description: "path to the file, relative to the project root"},
			"old_string": {Type: "string", Description: "the exact text to replace; must occur exactly once in the file"},
			"new_string": {Type: "string", Description: "the text to replace old_string with"},
		},
		Required: []string{"path", "old_string", "new_string"},
	}
}

// editInput is the schema of Edit's Execute input.
type editInput struct {
	Path      string `json:"path"`
	OldString string `json:"old_string"`
	NewString string `json:"new_string"`
}

func (e *Edit) Execute(_ context.Context, input json.RawMessage) (tool.Result, error) {
	var in editInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("edit: invalid input: %v", err)}, nil
	}
	if in.Path == "" {
		return tool.Result{IsError: true, Text: "edit: invalid input: path is required"}, nil
	}
	if in.OldString == in.NewString {
		return tool.Result{IsError: true, Text: "edit: old_string and new_string are identical, nothing to do"}, nil
	}

	relPath, err := e.dir.rel(in.Path)
	if err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	data, err := e.dir.root.ReadFile(relPath)
	if err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}
	before := string(data)

	switch n := strings.Count(before, in.OldString); {
	case n == 0:
		return tool.Result{IsError: true, Text: fmt.Sprintf("edit: old_string not found in %s", in.Path)}, nil
	case n > 1:
		return tool.Result{
			IsError: true,
			Text:    fmt.Sprintf("edit: old_string matches %d locations in %s; add surrounding context to make it unique", n, in.Path),
		}, nil
	}

	after := strings.Replace(before, in.OldString, in.NewString, 1)
	if err := e.dir.root.WriteFile(relPath, []byte(after), 0o644); err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	return tool.Result{Text: renderDiff(in.Path, before, after)}, nil
}
