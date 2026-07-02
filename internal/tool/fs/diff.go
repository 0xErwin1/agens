package fs

import (
	"fmt"
	"strings"
)

// diffContext is the number of unchanged lines shown around a change, on
// each side, in a rendered diff.
const diffContext = 3

// renderDiff renders a minimal single-hunk unified-diff-style view of the
// change from before to after. It assumes the two inputs differ by exactly
// one contiguous run of lines, which is the shape produced by a single
// old_string/new_string replacement (the edit tool's only use of it):
// finding the longest common line prefix and suffix isolates that run
// directly, with no general diff algorithm needed. The rendering is a
// review aid, not patch input: a missing final newline is not marked.
func renderDiff(path, before, after string) string {
	beforeLines := splitLines(before)
	afterLines := splitLines(after)

	prefix := commonPrefixLen(beforeLines, afterLines)
	suffix := commonSuffixLen(beforeLines[prefix:], afterLines[prefix:])

	ctxBefore := min(diffContext, prefix)
	ctxAfter := min(diffContext, suffix)

	hunkStart := prefix - ctxBefore
	oldEnd := len(beforeLines) - suffix + ctxAfter
	newEnd := len(afterLines) - suffix + ctxAfter

	var b strings.Builder
	fmt.Fprintf(&b, "--- a/%s\n", path)
	fmt.Fprintf(&b, "+++ b/%s\n", path)
	fmt.Fprintf(&b, "@@ -%d,%d +%d,%d @@\n", hunkStart+1, oldEnd-hunkStart, hunkStart+1, newEnd-hunkStart)

	writeLines(&b, " ", beforeLines[hunkStart:prefix])
	writeLines(&b, "-", beforeLines[prefix:len(beforeLines)-suffix])
	writeLines(&b, "+", afterLines[prefix:len(afterLines)-suffix])
	writeLines(&b, " ", beforeLines[len(beforeLines)-suffix:len(beforeLines)-suffix+ctxAfter])

	return b.String()
}

// splitLines splits s into lines with no trailing newline characters. A
// trailing "\n" in s does not produce a phantom empty final line.
func splitLines(s string) []string {
	if s == "" {
		return nil
	}

	lines := strings.Split(s, "\n")
	if lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	return lines
}

// commonPrefixLen returns how many leading lines a and b share.
func commonPrefixLen(a, b []string) int {
	n := min(len(a), len(b))

	i := 0
	for i < n && a[i] == b[i] {
		i++
	}
	return i
}

// commonSuffixLen returns how many trailing lines a and b share.
func commonSuffixLen(a, b []string) int {
	n := min(len(a), len(b))

	i := 0
	for i < n && a[len(a)-1-i] == b[len(b)-1-i] {
		i++
	}
	return i
}

// writeLines writes each of lines to b, prefixed with marker and followed
// by a newline.
func writeLines(b *strings.Builder, marker string, lines []string) {
	for _, l := range lines {
		fmt.Fprintf(b, "%s%s\n", marker, l)
	}
}
