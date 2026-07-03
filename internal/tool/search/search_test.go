package search

import (
	"errors"
	"strings"
	"testing"
)

func TestCapped_Add(t *testing.T) {
	t.Run("under cap keeps all items and adds no notice", func(t *testing.T) {
		c := newCapped(5, 1<<20, "matches")

		for i := 0; i < 3; i++ {
			if !c.add("item") {
				t.Fatalf("add() = false, want true (under cap)")
			}
		}

		if c.empty() {
			t.Fatalf("empty() = true, want false")
		}
		want := "item\nitem\nitem"
		if got := c.text(); got != want {
			t.Fatalf("text() = %q, want %q", got, want)
		}
	})

	t.Run("exactly maxItems adds no notice", func(t *testing.T) {
		c := newCapped(3, 1<<20, "matches")

		for i := 0; i < 3; i++ {
			if !c.add("item") {
				t.Fatalf("add() = false at item %d, want true", i)
			}
		}

		want := "item\nitem\nitem"
		if got := c.text(); got != want {
			t.Fatalf("text() = %q, want %q", got, want)
		}
	})

	t.Run("over maxItems truncates with count notice and bounds items", func(t *testing.T) {
		c := newCapped(3, 1<<20, "matches")

		for i := 0; i < 3; i++ {
			if !c.add("item") {
				t.Fatalf("add() = false at item %d, want true", i)
			}
		}
		if c.add("one too many") {
			t.Fatalf("add() = true for the item that trips the cap, want false")
		}

		const notice = "\n[output truncated after 3 matches]"
		got := c.text()
		if !strings.HasSuffix(got, notice) {
			t.Fatalf("text() = %q, want suffix %q", got, notice)
		}
		content := strings.TrimSuffix(got, notice)
		if strings.Count(content, "item") != 3 {
			t.Fatalf("text() content = %q, want exactly 3 items", content)
		}
	})

	t.Run("over maxOutputBytes truncates with byte-cap notice", func(t *testing.T) {
		c := newCapped(1000, 10, "paths")

		if !c.add("12345") {
			t.Fatalf("add() = false, want true (well under byte cap)")
		}
		if c.add("1234567890") {
			t.Fatalf("add() = true for an item that overflows the byte cap, want false")
		}

		const notice = "\n[output truncated after 100 KiB]"
		got := c.text()
		if !strings.HasSuffix(got, notice) {
			t.Fatalf("text() = %q, want suffix %q", got, notice)
		}
		content := strings.TrimSuffix(got, notice)
		if content != "12345" {
			t.Fatalf("text() content = %q, want %q", content, "12345")
		}
	})

	t.Run("empty accumulator", func(t *testing.T) {
		c := newCapped(5, 1<<20, "matches")

		if !c.empty() {
			t.Fatalf("empty() = false, want true")
		}
		if got := c.text(); got != "" {
			t.Fatalf("text() = %q, want empty string", got)
		}
	})
}

// TestCapped_ProductionLimits confirms the shared caps grep and glob will
// both accumulate against: 1000 items or 100 KiB of text, whichever trips
// first.
func TestCapped_ProductionLimits(t *testing.T) {
	if maxItems != 1000 {
		t.Fatalf("maxItems = %d, want 1000", maxItems)
	}
	if maxOutputBytes != 100<<10 {
		t.Fatalf("maxOutputBytes = %d, want %d", maxOutputBytes, 100<<10)
	}

	c := newCapped(maxItems, maxOutputBytes, "matches")
	for i := 0; i < maxItems; i++ {
		if !c.add("x") {
			t.Fatalf("add() = false at item %d, want true (within production item cap)", i)
		}
	}
	if c.add("one too many") {
		t.Fatalf("add() = true past the production item cap, want false")
	}
}

// TestErrCapReached confirms errCapReached is a stable sentinel that a walk
// callback can return and a caller can detect with errors.Is, distinct from
// an ordinary walk error.
func TestErrCapReached(t *testing.T) {
	if errCapReached == nil {
		t.Fatalf("errCapReached = nil, want a non-nil sentinel error")
	}
	if !errors.Is(errCapReached, errCapReached) {
		t.Fatalf("errors.Is(errCapReached, errCapReached) = false, want true")
	}
	if errors.Is(errors.New("some other walk error"), errCapReached) {
		t.Fatalf("errors.Is(otherErr, errCapReached) = true, want false")
	}
}

func TestFinishWalk(t *testing.T) {
	t.Run("no items and no cap reports no matches", func(t *testing.T) {
		out := newCapped(5, 1<<20, "matches")

		got := finishWalk(out, nil)
		if got.IsError {
			t.Fatalf("IsError = true, want false")
		}
		if got.Text != "no matches" {
			t.Fatalf("Text = %q, want %q", got.Text, "no matches")
		}
	})

	t.Run("items accumulated reports accumulated text", func(t *testing.T) {
		out := newCapped(5, 1<<20, "matches")
		out.add("a")
		out.add("b")

		got := finishWalk(out, nil)
		if got.Text != "a\nb" {
			t.Fatalf("Text = %q, want %q", got.Text, "a\nb")
		}
	})

	t.Run("first item alone trips the byte cap reports the notice, not no matches", func(t *testing.T) {
		out := newCapped(1000, 5, "matches")

		if out.add("this line alone exceeds the byte cap") {
			t.Fatalf("add() = true, want false (line alone overflows maxBytes)")
		}

		got := finishWalk(out, errCapReached)
		if got.Text == "no matches" {
			t.Fatalf("Text = %q, want the truncation notice, not \"no matches\"", got.Text)
		}
		const notice = "\n[output truncated after 100 KiB]"
		if !strings.HasSuffix(got.Text, notice) {
			t.Fatalf("Text = %q, want suffix %q", got.Text, notice)
		}
	})
}

func TestValidateRel(t *testing.T) {
	tests := []struct {
		name    string
		p       string
		wantErr bool
	}{
		{name: "empty is ok", p: "", wantErr: false},
		{name: "clean relative path is ok", p: "sub/dir", wantErr: false},
		{name: "single relative segment is ok", p: "file.go", wantErr: false},
		{name: "leading slash is rejected", p: "/abs/outside", wantErr: true},
		{name: "leading parent traversal is rejected", p: "../x", wantErr: true},
		{name: "embedded parent traversal is rejected", p: "a/../../b", wantErr: true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := validateRel("grep", "path", tt.p)
			if (err != nil) != tt.wantErr {
				t.Fatalf("validateRel(%q) error = %v, wantErr %v", tt.p, err, tt.wantErr)
			}
		})
	}
}
