package webfetch

import (
	"strings"
	"testing"
)

func TestIsHTML(t *testing.T) {
	tests := []struct {
		name        string
		contentType string
		want        bool
	}{
		{name: "plain text/html", contentType: "text/html", want: true},
		{name: "text/html with charset", contentType: "text/html; charset=utf-8", want: true},
		{name: "json", contentType: "application/json", want: false},
		{name: "malformed", contentType: ";;;not-a-media-type", want: false},
		{name: "empty", contentType: "", want: false},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := isHTML(tc.contentType); got != tc.want {
				t.Fatalf("isHTML(%q) = %v, want %v", tc.contentType, got, tc.want)
			}
		})
	}
}

func TestExtractText(t *testing.T) {
	tests := []struct {
		name         string
		html         string
		wantContains []string
		wantExcludes []string
	}{
		{
			name:         "script and style skipped",
			html:         `<html><head><style>body{color:red}</style></head><body><script>alert(1)</script><p>Hello world</p></body></html>`,
			wantContains: []string{"Hello world"},
			wantExcludes: []string{"color:red", "alert(1)"},
		},
		{
			name:         "whitespace collapsed",
			html:         "<p>Hello    \n\n  world</p>",
			wantContains: []string{"Hello world"},
		},
		{
			name:         "block tags produce newlines",
			html:         `<div>First</div><div>Second</div>`,
			wantContains: []string{"First", "Second"},
		},
		{
			name:         "entities decoded",
			html:         `<p>Tom &amp; Jerry</p>`,
			wantContains: []string{"Tom & Jerry"},
			wantExcludes: []string{"&amp;"},
		},
		{
			name:         "malformed HTML tolerated",
			html:         `<p>Unterminated paragraph <div>nested but broken`,
			wantContains: []string{"Unterminated paragraph", "nested but broken"},
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got := extractText([]byte(tc.html))

			for _, want := range tc.wantContains {
				if !strings.Contains(got, want) {
					t.Fatalf("extractText(%q) = %q, want it to contain %q", tc.html, got, want)
				}
			}
			for _, exclude := range tc.wantExcludes {
				if strings.Contains(got, exclude) {
					t.Fatalf("extractText(%q) = %q, want it to NOT contain %q", tc.html, got, exclude)
				}
			}
		})
	}
}

func TestExtractText_BlockTagNewlineSeparation(t *testing.T) {
	got := extractText([]byte(`<p>First paragraph</p><p>Second paragraph</p>`))
	if !strings.Contains(got, "First paragraph\n") {
		t.Fatalf("extractText() = %q, want a newline after the first block element", got)
	}
}

func TestExtractText_NoPanicOnMalformedInput(t *testing.T) {
	defer func() {
		if r := recover(); r != nil {
			t.Fatalf("extractText panicked on malformed input: %v", r)
		}
	}()
	extractText([]byte(`<p>unterminated <script>alert(1)`))
}
