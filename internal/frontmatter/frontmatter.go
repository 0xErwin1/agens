// Package frontmatter splits a leading YAML `---` block from a markdown body.
// It is the shared parsing seam for the file-backed authoring surfaces — agent
// definitions, skills, and slash-commands — so the fenced-block handling stays
// byte-for-byte identical across all of them.
package frontmatter

import (
	"bufio"
	"bytes"
	"io"
	"strings"
)

// Split separates a leading `---`-delimited YAML block from the markdown body.
// It returns (nil, data) when the file does not open with a `---` line or the
// block is never closed, so a plain markdown file is treated as a body with no
// frontmatter rather than an error.
func Split(data []byte) (front, body []byte) {
	reader := bufio.NewReader(bytes.NewReader(data))

	first, err := reader.ReadString('\n')
	if strings.TrimRight(first, "\r\n") != "---" {
		return nil, data
	}
	if err != nil {
		return nil, data
	}

	var collected bytes.Buffer
	for {
		line, err := reader.ReadString('\n')
		if strings.TrimRight(line, "\r\n") == "---" {
			rest, _ := io.ReadAll(reader)
			return collected.Bytes(), rest
		}
		collected.WriteString(line)
		if err != nil {
			return nil, data
		}
	}
}
