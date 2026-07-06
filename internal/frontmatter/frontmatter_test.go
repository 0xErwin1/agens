package frontmatter

import (
	"strings"
	"testing"
)

func TestSplit_FrontmatterAndBody(t *testing.T) {
	front, body := Split([]byte("---\nname: research\n---\nthe body\n"))

	if front == nil {
		t.Fatal("front = nil, want the frontmatter bytes")
	}
	if !strings.Contains(string(front), "name: research") {
		t.Fatalf("front = %q, want the yaml block", string(front))
	}
	if strings.TrimSpace(string(body)) != "the body" {
		t.Fatalf("body = %q, want the content after the closing fence", string(body))
	}
}

func TestSplit_NoOpeningFenceIsAllBody(t *testing.T) {
	data := []byte("# Title\n\njust markdown\n")

	front, body := Split(data)
	if front != nil {
		t.Fatalf("front = %q, want nil for a file without a leading fence", string(front))
	}
	if string(body) != string(data) {
		t.Fatalf("body = %q, want the whole file unchanged", string(body))
	}
}

func TestSplit_UnclosedFenceIsAllBody(t *testing.T) {
	data := []byte("---\nname: x\nno closing fence\n")

	front, body := Split(data)
	if front != nil {
		t.Fatalf("front = %q, want nil when the block is never closed", string(front))
	}
	if string(body) != string(data) {
		t.Fatalf("body = %q, want the raw file treated as body", string(body))
	}
}

func TestSplit_HandlesCRLF(t *testing.T) {
	front, body := Split([]byte("---\r\nname: y\r\n---\r\nbody\r\n"))

	if !strings.Contains(string(front), "name: y") {
		t.Fatalf("front = %q, want the yaml block with CRLF fences recognized", string(front))
	}
	if strings.TrimSpace(string(body)) != "body" {
		t.Fatalf("body = %q, want the body after a CRLF closing fence", string(body))
	}
}
