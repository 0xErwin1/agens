package skill

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/google/jsonschema-go/jsonschema"

	"github.com/0xErwin1/agens/internal/tool"
)

// maxFileBytes caps a level-3 bundled-file read so a large asset cannot flood
// the context; a file past this size is returned truncated with a trailing note.
const maxFileBytes = 256 * 1024

// Tool is the built-in `skill` tool that drives progressive disclosure past
// level 1 (the name+description already injected into the system prompt): called
// with just a skill name it returns that skill's full SKILL.md body (level 2);
// called additionally with a path it returns a bundled file under the skill's
// directory (level 3). It is read-only and each read is confined to the named
// skill's own directory, so it can reach global skills that live outside the
// project worktree without widening the file tools' confinement.
type Tool struct {
	set *Set
}

// NewTool builds the skill tool over set. The composition root registers it only
// when set is non-empty, so the tool's enum always advertises at least one skill.
func NewTool(set *Set) *Tool {
	return &Tool{set: set}
}

func (t *Tool) Name() string { return "skill" }

func (t *Tool) Description() string {
	return "Load an available skill's full instructions. When a task matches a skill's " +
		"description, call this with the skill's name to read its SKILL.md, then optionally " +
		"pass a path to read a bundled file (a script, reference, or asset) under that skill."
}

type skillInput struct {
	Name string `json:"name"`
	Path string `json:"path,omitempty"`
}

func (t *Tool) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"name": {
				Type:        "string",
				Enum:        t.nameEnum(),
				Description: "the skill to load, one of the available skill names",
			},
			"path": {
				Type: "string",
				Description: "optional file to read relative to the skill's directory " +
					"(for example references/guide.md); omit to read the skill's SKILL.md",
			},
		},
		Required: []string{"name"},
	}
}

func (t *Tool) Execute(ctx context.Context, input json.RawMessage) (tool.Result, error) {
	var in skillInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("skill: invalid input: %v", err)}, nil
	}

	name := strings.TrimSpace(in.Name)
	sk, ok := t.set.ByName(name)
	if !ok {
		return tool.Result{
			IsError: true,
			Text:    fmt.Sprintf("skill: unknown skill %q; available: %s", in.Name, strings.Join(t.names(), ", ")),
		}, nil
	}

	if strings.TrimSpace(in.Path) == "" {
		return tool.Result{Text: renderManifest(sk)}, nil
	}

	text, toolErr := readBundledFile(sk, in.Path)
	if toolErr != "" {
		return tool.Result{IsError: true, Text: toolErr}, nil
	}
	return tool.Result{Text: text}, nil
}

// names lists the available skill names in discovery order.
func (t *Tool) names() []string {
	skills := t.set.All()
	names := make([]string, len(skills))
	for i, sk := range skills {
		names[i] = sk.Name
	}
	return names
}

// nameEnum lists the available skill names as the schema's enum constraint.
func (t *Tool) nameEnum() []any {
	names := t.names()
	out := make([]any, len(names))
	for i, n := range names {
		out[i] = n
	}
	return out
}

// renderManifest is the level-2 payload: the skill's full SKILL.md body preceded
// by a header naming the skill and its directory, so the model knows the root
// that a follow-up path read (level 3) resolves against.
func renderManifest(sk Skill) string {
	return fmt.Sprintf("Skill: %s\nDirectory: %s\n\n%s", sk.Name, sk.Dir, sk.Body)
}

// readBundledFile is the level-3 read: it returns the content of path resolved
// under the skill's own directory. The read is confined by os.Root, so a path
// escaping the directory (parent traversal, an absolute path, or a symlink
// resolving outside) is rejected by the operating system. It returns a non-empty
// tool-error string (to surface to the model) on any failure. Content past
// maxFileBytes is truncated with a trailing note.
func readBundledFile(sk Skill, path string) (text, toolErr string) {
	root, err := os.OpenRoot(sk.Dir)
	if err != nil {
		return "", fmt.Sprintf("skill: cannot open skill directory %s: %v", sk.Dir, err)
	}
	defer func() { _ = root.Close() }()

	f, err := root.Open(path)
	if err != nil {
		return "", fmt.Sprintf("skill: cannot read %q under skill %q: %v", path, sk.Name, err)
	}
	defer func() { _ = f.Close() }()

	data, err := io.ReadAll(io.LimitReader(f, maxFileBytes+1))
	if err != nil {
		return "", fmt.Sprintf("skill: cannot read %q under skill %q: %v", path, sk.Name, err)
	}

	if len(data) > maxFileBytes {
		return string(data[:maxFileBytes]) + "\n\n[truncated: file exceeds the read limit]", ""
	}
	return string(data), ""
}
