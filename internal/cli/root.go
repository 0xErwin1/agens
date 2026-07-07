package cli

import (
	"github.com/spf13/cobra"

	"github.com/0xErwin1/agens/internal/version"
)

func NewRootCommand() *cobra.Command {
	return newRootCommand(defaultBuildTUI, defaultRunTUI)
}

// newRootCommand assembles the command tree with the TUI as the root's default
// action, so bare `agens` opens the interactive UI while the subcommands remain
// available. The build and run seams are threaded to the root's TUI action so
// tests can drive it without config, auth, a network, or a TTY.
func newRootCommand(build tuiLoopBuilder, run tuiRunner) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "agens",
		Short: "Agens is a coding agent CLI",
		Long:  "Agens is a coding agent CLI. Run it with no arguments to open the interactive terminal UI, or pass a session id to resume that conversation.",
		Args:  cobra.MaximumNArgs(1),
	}

	cmd.Version = version.Info()

	configureRootTUI(cmd, build, run)

	cmd.AddCommand(newAuthCommand())
	cmd.AddCommand(newConfigCommand())
	cmd.AddCommand(newChatCommand())
	cmd.AddCommand(newModelsCommand())
	return cmd
}
