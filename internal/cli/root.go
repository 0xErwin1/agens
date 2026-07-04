package cli

import (
	"github.com/spf13/cobra"

	"github.com/iperez/agens/internal/version"
)

func NewRootCommand() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "agens",
		Short: "Agens is a coding agent CLI",
		Long:  "Agens is a coding agent CLI with a headless core and future TUI support.",
		RunE: func(cmd *cobra.Command, _ []string) error {
			return cmd.Help()
		},
	}

	cmd.Version = version.Info()
	cmd.AddCommand(newAuthCommand())
	cmd.AddCommand(newConfigCommand())
	cmd.AddCommand(newChatCommand())
	return cmd
}
