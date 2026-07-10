package cli

import (
	"fmt"
	"io"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"github.com/0xErwin1/agens/internal/agent"
	"github.com/0xErwin1/agens/internal/auth"
	"github.com/0xErwin1/agens/internal/config"
	"github.com/0xErwin1/agens/internal/provider"
)

// providerBuilder resolves an agent.Options into a provider.Provider. It is
// the dependency-injection seam that lets tests exercise the models command
// against a fake provider without touching config, auth, or the network.
type providerBuilder func(opts agent.Options) (provider.Provider, error)

func newModelsCommand() *cobra.Command {
	return newModelsCommandWithBuilder(defaultBuildProvider)
}

func newModelsCommandWithBuilder(build providerBuilder) *cobra.Command {
	var opts agent.Options

	cmd := &cobra.Command{
		Use:   "models",
		Short: "List the models available from the configured provider",
		RunE: func(cmd *cobra.Command, _ []string) error {
			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()

			p, err := build(opts)
			if err != nil {
				return fmt.Errorf("models: %w", err)
			}

			models, err := p.Models(ctx)
			if err != nil {
				return fmt.Errorf("models: %w", err)
			}

			return writeModelsTable(cmd.OutOrStdout(), models)
		},
	}

	return cmd
}

// writeModelsTable renders models as an aligned ID/NAME/CONTEXT/PRICE table
// to w, or a single explanatory line when models is empty. A model's context
// window is only printed when positive, and its price only when the
// registry supplied both input and output cost; either unknown value renders
// as "-" instead of a misleading 0 or $0.
func writeModelsTable(w io.Writer, models []provider.ModelInfo) error {
	if len(models) == 0 {
		_, err := fmt.Fprintln(w, "No models available.")
		return err
	}

	tw := tabwriter.NewWriter(w, 0, 4, 2, ' ', 0)

	if _, err := fmt.Fprintln(tw, "ID\tNAME\tCONTEXT\tPRICE"); err != nil {
		return err
	}

	for _, m := range models {
		contextWindow := "-"
		if m.ContextWindow > 0 {
			contextWindow = strconv.Itoa(m.ContextWindow)
		}

		if _, err := fmt.Fprintf(tw, "%s\t%s\t%s\t%s\n", m.ID, m.DisplayName, contextWindow, formatPrice(m)); err != nil {
			return err
		}
	}

	return tw.Flush()
}

// formatPrice renders a model's input/output cost as "$in/$out" per million
// tokens. Pricing is only ever known as a pair (see modelregistry.Enrich), so
// checking either field for nil is enough to detect the unknown case.
func formatPrice(m provider.ModelInfo) string {
	if m.InputCostPerMTok == nil || m.OutputCostPerMTok == nil {
		return "-"
	}
	return fmt.Sprintf("$%.2f/$%.2f", *m.InputCostPerMTok, *m.OutputCostPerMTok)
}

// defaultBuildProvider loads config + credentials from disk, then delegates
// to agent.BuildProvider (mirrors chat.go's defaultBuildLoop).
func defaultBuildProvider(opts agent.Options) (provider.Provider, error) {
	loaded, err := config.Load()
	if err != nil {
		return nil, fmt.Errorf("models: load config: %w", err)
	}

	creds, err := auth.Load(auth.DefaultPath())
	if err != nil {
		return nil, fmt.Errorf("models: %w", err)
	}

	if opts.ProjectRoot == "" {
		opts.ProjectRoot = loaded.ProjectRoot
	}

	p, err := agent.BuildProvider(loaded.Config, creds, opts)
	if err != nil {
		return nil, fmt.Errorf("models: %w", err)
	}
	return p, nil
}
