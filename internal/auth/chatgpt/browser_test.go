package chatgpt

import (
	"bytes"
	"errors"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestOpenBrowser_WritesURLBeforeExecAttempt(t *testing.T) {
	var out bytes.Buffer
	execCalled := false

	original := browserCommand
	t.Cleanup(func() { browserCommand = original })
	browserCommand = func(url string) *exec.Cmd {
		execCalled = true
		if out.Len() == 0 {
			t.Fatal("browserCommand() invoked before the URL was written to out")
		}
		return exec.Command(filepath.Join(t.TempDir(), "nonexistent-browser-binary"))
	}

	const authorizeURLValue = "https://auth.openai.com/oauth/authorize?state=xyz"
	if err := openBrowser(&out, authorizeURLValue); err != nil {
		t.Fatalf("openBrowser() error = %v", err)
	}
	if !execCalled {
		t.Fatal("openBrowser() never invoked browserCommand")
	}
	if !strings.Contains(out.String(), authorizeURLValue) {
		t.Fatalf("out = %q, want it to contain the authorize URL", out.String())
	}
}

func TestOpenBrowser_ExecFailureDoesNotPropagate(t *testing.T) {
	var out bytes.Buffer

	original := browserCommand
	t.Cleanup(func() { browserCommand = original })
	browserCommand = func(url string) *exec.Cmd {
		return exec.Command(filepath.Join(t.TempDir(), "nonexistent-browser-binary"))
	}

	if err := openBrowser(&out, "https://example.com/authorize"); err != nil {
		t.Fatalf("openBrowser() error = %v, want nil even when the browser opener fails", err)
	}
}

type errWriter struct{}

func (errWriter) Write([]byte) (int, error) {
	return 0, errors.New("write failed")
}

func TestOpenBrowser_WriteFailurePropagates(t *testing.T) {
	original := browserCommand
	t.Cleanup(func() { browserCommand = original })
	browserCommand = func(url string) *exec.Cmd {
		t.Fatal("browserCommand() invoked despite the URL write having failed")
		return nil
	}

	err := openBrowser(errWriter{}, "https://example.com/authorize")
	if err == nil {
		t.Fatal("openBrowser() error = nil, want error when writing the URL fails")
	}
}
