package chatgpt

import (
	"fmt"
	"io"
	"os/exec"
	"runtime"
)

// browserCommand builds the OS-specific command used to open a URL in the
// user's default browser. It is a package-level var, following the same
// testability seam as openControllingTTY in internal/cli/auth.go, so tests
// can force exec failures deterministically without launching a real
// browser.
var browserCommand = func(url string) *exec.Cmd {
	switch runtime.GOOS {
	case "darwin":
		return exec.Command("open", url)
	case "windows":
		return exec.Command("rundll32", "url.dll,FileProtocolHandler", url)
	default:
		return exec.Command("xdg-open", url)
	}
}

// openBrowser writes url to out so the user can always open it manually,
// then makes a best-effort attempt to launch it in the system's default
// browser. A failure to launch the browser (missing opener, no display,
// etc.) is swallowed rather than returned: the URL was already printed to
// out, so the caller's login flow can proceed via manual copy-paste. The
// only error openBrowser can return is a failure to write to out itself.
func openBrowser(out io.Writer, url string) error {
	if _, err := fmt.Fprintln(out, url); err != nil {
		return fmt.Errorf("chatgpt: write authorize URL: %w", err)
	}

	_ = browserCommand(url).Run()
	return nil
}
