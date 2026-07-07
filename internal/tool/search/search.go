// Package search provides read-only grep and glob tool.Tool implementations
// that operate over a confined fs.FS — the same os.Root-backed filesystem
// exposed by internal/tool/fs's Dir.FS() — plus the accumulation and input
// validation helpers shared by both tools.
//
// Both tools are best-effort: unreadable, binary, or oversized files are
// silently skipped rather than surfaced as errors, and output is capped
// with a truncation notice, mirroring the posture of internal/tool/bash's
// output capping.
package search

import (
	"errors"
	"fmt"
	"strings"

	"github.com/0xErwin1/agens/internal/tool"
)

const (
	maxItems       = 1000
	maxOutputBytes = 100 << 10

	// maxFileBytes bounds the size of a file grep will read into memory; a
	// larger file is silently skipped rather than searched.
	maxFileBytes = 10 << 20

	// binarySniffLen is the number of leading bytes grep inspects for a NUL
	// byte to heuristically classify a file as binary and skip it, mirroring
	// git's own binary-detection heuristic.
	binarySniffLen = 8192
)

// errCapReached is returned by a walk callback to stop the underlying walk
// early once a capped accumulator has reached its item or byte limit. It is
// not itself a tool-level failure: the post-walk error handling in grep.go
// and glob.go recognizes it and reports success with the accumulated,
// truncated text.
var errCapReached = errors.New("search: output cap reached")

// capped accumulates newline-joined items — grep matches or glob paths — up
// to maxItems or maxBytes, whichever trips first. Once full, add stops
// accepting items and records a truncation notice for text to append.
type capped struct {
	buf      strings.Builder
	maxItems int
	maxBytes int
	items    int
	itemNoun string
	notice   string
}

// newCapped returns a capped accumulator bounded by maxItems and maxBytes,
// using itemNoun ("matches" or "paths") in the truncation notice.
func newCapped(maxItems, maxBytes int, itemNoun string) *capped {
	return &capped{maxItems: maxItems, maxBytes: maxBytes, itemNoun: itemNoun}
}

// add appends line to the accumulator, newline-joining with any prior
// content, and reports whether it was accepted. Once either cap trips, add
// rejects the item that would have exceeded it, records a truncation
// notice, and rejects every subsequent call.
func (c *capped) add(line string) bool {
	if c.notice != "" {
		return false
	}

	if c.items >= c.maxItems {
		c.notice = fmt.Sprintf("\n[output truncated after %d %s]", c.maxItems, c.itemNoun)
		return false
	}

	sep := 0
	if c.buf.Len() > 0 {
		sep = 1
	}
	if c.buf.Len()+sep+len(line) > c.maxBytes {
		c.notice = "\n[output truncated after 100 KiB]"
		return false
	}

	if sep > 0 {
		c.buf.WriteByte('\n')
	}
	c.buf.WriteString(line)
	c.items++
	return true
}

// text returns the accumulated content with the truncation notice appended,
// if any item was dropped.
func (c *capped) text() string {
	return c.buf.String() + c.notice
}

// empty reports whether no items were ever accepted.
func (c *capped) empty() bool {
	return c.items == 0
}

// finishWalk maps the outcome of a completed accumulation walk to a
// tool.Result. walkErr must already be known to be a non-failure: nil, or
// errCapReached because a cap was hit. A walk that accepted no items and
// never hit a cap reports "no matches"; one that trips a cap on its very
// first, oversized item still reports the truncation notice rather than
// falling through to "no matches", since out.text() carries that notice even
// when out is otherwise empty.
func finishWalk(out *capped, walkErr error) tool.Result {
	if out.empty() && !errors.Is(walkErr, errCapReached) {
		return tool.Result{Text: "no matches"}
	}
	return tool.Result{Text: out.text()}
}

// validateRel rejects an explicit path/glob/pattern input that would escape
// the confined root: a leading "/" (absolute) or any ".." path segment. An
// empty p is always accepted (it means "no scoping value supplied"). This
// is a defense-in-depth guard ahead of the os.Root-backed fs.FS boundary,
// which never reads or lists outside the root regardless.
func validateRel(toolName, field, p string) error {
	if p == "" {
		return nil
	}

	if strings.HasPrefix(p, "/") {
		return fmt.Errorf("%s: %s must be relative to the project root, got %q", toolName, field, p)
	}

	for _, seg := range strings.Split(p, "/") {
		if seg == ".." {
			return fmt.Errorf("%s: %s must be relative to the project root, got %q", toolName, field, p)
		}
	}

	return nil
}
