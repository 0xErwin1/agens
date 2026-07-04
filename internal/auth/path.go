package auth

import (
	"path/filepath"

	"github.com/iperez/agens/internal/config"
)

// DefaultPath returns the conventional on-disk location of the
// credentials file, resolved under the same config-home directory
// (AGENS_CONFIG_HOME, then XDG_CONFIG_HOME, then the user's home
// directory) used by the rest of the CLI.
func DefaultPath() string {
	return filepath.Join(config.HomeDir(), "auth.json")
}
