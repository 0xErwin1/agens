package cli

import (
	"fmt"
	"io"

	"github.com/spf13/cobra"

	"github.com/iperez/agens/internal/config"
)

func newConfigCommand() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "config",
		Short: "Inspect Agens configuration",
	}
	cmd.AddCommand(newConfigDoctorCommand())
	return cmd
}

func newConfigDoctorCommand() *cobra.Command {
	return &cobra.Command{
		Use:   "doctor",
		Short: "Validate and explain loaded configuration",
		RunE: func(cmd *cobra.Command, _ []string) error {
			loaded, err := config.Load()
			if err != nil {
				if writeErr := writeInvalidConfig(cmd.ErrOrStderr(), err); writeErr != nil {
					return writeErr
				}
				return err
			}
			_, writeErr := io.WriteString(cmd.OutOrStdout(), config.DoctorReport(loaded))
			return writeErr
		},
	}
}

func writeInvalidConfig(writer io.Writer, err error) error {
	_, writeErr := fmt.Fprintf(writer, "Agens config doctor\nStatus:  invalid\nError:   %v\n", err)
	return writeErr
}
