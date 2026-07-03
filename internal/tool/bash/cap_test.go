package bash

import (
	"strings"
	"testing"
)

const testCapLimit = 100 * 1024

func TestCappedBuffer(t *testing.T) {
	tests := []struct {
		name          string
		size          int
		wantTruncated bool
	}{
		{name: "well under cap", size: 10, wantTruncated: false},
		{name: "just under cap", size: testCapLimit - 1, wantTruncated: false},
		{name: "exactly at cap", size: testCapLimit, wantTruncated: false},
		{name: "just over cap", size: testCapLimit + 1, wantTruncated: true},
		{name: "well over cap", size: testCapLimit * 2, wantTruncated: true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			c := newCappedBuffer(testCapLimit)
			data := []byte(strings.Repeat("x", tt.size))

			n, err := c.Write(data)
			if err != nil {
				t.Fatalf("Write() error = %v, want nil regardless of size", err)
			}
			if n != len(data) {
				t.Fatalf("Write() n = %d, want %d", n, len(data))
			}

			wantLen := tt.size
			if wantLen > testCapLimit {
				wantLen = testCapLimit
			}

			text := c.Text()
			content := text
			if tt.wantTruncated {
				const notice = "\n[output truncated after 100 KiB]"
				if !strings.HasSuffix(text, notice) {
					t.Fatalf("Text() = %q, want suffix %q", text, notice)
				}
				content = strings.TrimSuffix(text, notice)
			}

			if len(content) != wantLen {
				t.Fatalf("Text() content length = %d, want %d", len(content), wantLen)
			}
		})
	}
}
