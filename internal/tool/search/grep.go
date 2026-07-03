package search

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"path"
	"regexp"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
	"github.com/google/jsonschema-go/jsonschema"
	"github.com/iperez/agens/internal/tool"
)

// Grep implements the "grep" tool: it searches file contents for a Go RE2
// regular expression, confined to fsys.
type Grep struct {
	fsys fs.FS
}

// NewGrep returns a Grep tool operating over fsys. It panics if fsys is nil,
// since a nil fsys is a wiring bug the composition root must fail fast on.
func NewGrep(fsys fs.FS) *Grep {
	if fsys == nil {
		panic("search: NewGrep called with a nil fs.FS")
	}
	return &Grep{fsys: fsys}
}

func (g *Grep) Name() string { return "grep" }

func (g *Grep) Description() string {
	return "Search file contents for a Go RE2 regular expression (no backreferences or lookahead), " +
		"returning path:line:text matches, confined to the project root."
}

func (g *Grep) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"pattern": {
				Type:        "string",
				Description: "Go RE2 regular expression to search file contents for (no backreferences or lookahead)",
			},
			"path": {
				Type:        "string",
				Description: "directory to scope the search to, relative to the project root (default: entire project)",
			},
			"glob": {
				Type: "string",
				Description: `glob filter for which files to search, e.g. "**/*.go", relative to path ` +
					`(default: all files)`,
			},
			"case_insensitive": {
				Type:        "boolean",
				Description: "match case-insensitively (default: false)",
			},
		},
		Required: []string{"pattern"},
	}
}

// grepInput is the schema of Grep's Execute input.
type grepInput struct {
	Pattern         string `json:"pattern"`
	Path            string `json:"path"`
	Glob            string `json:"glob"`
	CaseInsensitive bool   `json:"case_insensitive"`
}

func (g *Grep) Execute(_ context.Context, input json.RawMessage) (tool.Result, error) {
	var in grepInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("grep: invalid input: %v", err)}, nil
	}
	if in.Pattern == "" {
		return tool.Result{IsError: true, Text: "grep: invalid input: pattern is required"}, nil
	}
	if err := validateRel("grep", "path", in.Path); err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}
	if err := validateRel("grep", "glob", in.Glob); err != nil {
		return tool.Result{IsError: true, Text: err.Error()}, nil
	}

	re, err := compilePattern(in.Pattern, in.CaseInsensitive)
	if err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("grep: invalid pattern: %v", err)}, nil
	}

	scope := in.Path
	if scope == "" {
		scope = "."
	}
	if scope != "." {
		info, statErr := fs.Stat(g.fsys, scope)
		if statErr != nil || !info.IsDir() {
			return tool.Result{IsError: true, Text: fmt.Sprintf("grep: path %q is not a directory in the project", in.Path)}, nil
		}
	}

	out := newCapped(maxItems, maxOutputBytes, "matches")
	walkErr := g.walk(re, scope, in.Glob, out)

	if walkErr != nil && !errors.Is(walkErr, errCapReached) {
		if errors.Is(walkErr, doublestar.ErrBadPattern) {
			return tool.Result{IsError: true, Text: fmt.Sprintf("grep: invalid glob pattern: %v", walkErr)}, nil
		}
		return tool.Result{IsError: true, Text: fmt.Sprintf("grep: %v", walkErr)}, nil
	}

	return finishWalk(out, walkErr), nil
}

// compilePattern compiles pattern as a Go RE2 regular expression, prepending
// the "(?i)" flag when caseInsensitive is set.
func compilePattern(pattern string, caseInsensitive bool) (*regexp.Regexp, error) {
	if caseInsensitive {
		pattern = "(?i)" + pattern
	}
	return regexp.Compile(pattern)
}

// walk searches scope for files matching glob (or every file under scope,
// if glob is empty), applying re to each line via grepFile. It reports the
// first non-nil error the walk produces: nil on completion, errCapReached
// once out's cap is hit, or a genuine walk failure (for example a malformed
// glob).
func (g *Grep) walk(re *regexp.Regexp, scope, glob string, out *capped) error {
	if glob != "" {
		return doublestar.GlobWalk(g.fsys, path.Join(scope, glob), func(p string, d fs.DirEntry) error {
			if d.IsDir() {
				return nil
			}
			if !g.grepFile(re, p, out) {
				return errCapReached
			}
			return nil
		}, doublestar.WithNoHidden())
	}

	return fs.WalkDir(g.fsys, scope, func(p string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return nil
		}
		if p != scope && strings.HasPrefix(d.Name(), ".") {
			if d.IsDir() {
				return fs.SkipDir
			}
			return nil
		}
		if d.IsDir() {
			return nil
		}
		if !g.grepFile(re, p, out) {
			return errCapReached
		}
		return nil
	})
}

// grepFile searches p for lines matching re, appending each as
// "path:line:text" to out. It reports false once out's cap has been
// reached, signaling the caller to abort the walk. p is silently skipped
// (reported as true, no matches added) when it cannot be treated as a
// reasonably sized text file: a Stat failure (including an escaping
// symlink, which the confined fsys refuses to resolve), a non-regular file,
// a file larger than maxFileBytes, or a file whose first binarySniffLen
// bytes contain a NUL byte (heuristically binary).
func (g *Grep) grepFile(re *regexp.Regexp, p string, out *capped) bool {
	info, err := fs.Stat(g.fsys, p)
	if err != nil || !info.Mode().IsRegular() || info.Size() > maxFileBytes {
		return true
	}

	data, err := fs.ReadFile(g.fsys, p)
	if err != nil {
		return true
	}

	sniff := data
	if len(sniff) > binarySniffLen {
		sniff = sniff[:binarySniffLen]
	}
	if bytes.IndexByte(sniff, 0) != -1 {
		return true
	}

	for i, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSuffix(line, "\r")
		if !re.MatchString(line) {
			continue
		}
		if !out.add(fmt.Sprintf("%s:%d:%s", p, i+1, line)) {
			return false
		}
	}
	return true
}
