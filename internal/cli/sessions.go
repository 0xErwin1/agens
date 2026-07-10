package cli

import (
	"fmt"
	"io"
	"text/tabwriter"

	"github.com/spf13/cobra"

	"github.com/0xErwin1/agens/internal/config"
	"github.com/0xErwin1/agens/internal/session"
	"github.com/0xErwin1/agens/internal/session/sessiondb"
)

// sessionStore is the narrow port the sessions CLI needs from a session
// store: list metadata, delete by id, and release the underlying handle.
// *sessiondb.Store satisfies it.
type sessionStore interface {
	ListMeta() ([]session.Session, error)
	Delete(id string) error
	Close() error
}

// sessionStoreOpener opens a sessionStore and reports the current project
// root, the dependency-injection seam that lets tests drive the sessions
// command against a fake store without touching config or the filesystem.
type sessionStoreOpener func() (store sessionStore, project string, err error)

func newSessionsCommand() *cobra.Command {
	return newSessionsCommandWithOpener(defaultOpenSessionStore)
}

func newSessionsCommandWithOpener(open sessionStoreOpener) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "sessions",
		Short: "Manage saved conversation sessions",
	}

	cmd.AddCommand(newSessionsListCommand(open))
	cmd.AddCommand(newSessionsRmCommand(open))
	return cmd
}

func newSessionsListCommand(open sessionStoreOpener) *cobra.Command {
	var all bool

	cmd := &cobra.Command{
		Use:   "list",
		Short: "List saved sessions",
		RunE: func(cmd *cobra.Command, _ []string) error {
			store, project, err := open()
			if err != nil {
				return fmt.Errorf("sessions: %w", err)
			}
			defer func() { _ = store.Close() }()

			sessions, err := store.ListMeta()
			if err != nil {
				return fmt.Errorf("sessions: %w", err)
			}

			if !all {
				sessions = filterByProject(sessions, project)
			}

			return writeSessionsTable(cmd.OutOrStdout(), sessions)
		},
	}

	cmd.Flags().BoolVar(&all, "all", false, "list sessions across all projects instead of just the current one")
	return cmd
}

func newSessionsRmCommand(open sessionStoreOpener) *cobra.Command {
	return &cobra.Command{
		Use:   "rm <id>",
		Short: "Delete a saved session",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			store, _, err := open()
			if err != nil {
				return fmt.Errorf("sessions: %w", err)
			}
			defer func() { _ = store.Close() }()

			id := args[0]
			if err := store.Delete(id); err != nil {
				return fmt.Errorf("sessions: %w", err)
			}

			_, err = fmt.Fprintf(cmd.OutOrStdout(), "Deleted session %s.\n", id)
			return err
		},
	}
}

// filterByProject keeps only the sessions belonging to project, mirroring
// the TUI picker's client-side scoping (applySessionFilter in
// internal/tui/tui.go) since ListMeta has no server-side project filter.
func filterByProject(sessions []session.Session, project string) []session.Session {
	filtered := make([]session.Session, 0, len(sessions))
	for _, s := range sessions {
		if s.Project == project {
			filtered = append(filtered, s)
		}
	}
	return filtered
}

// writeSessionsTable renders sessions as an aligned ID/TITLE/PROJECT/AGENT/
// UPDATED table to w, or a single explanatory line when sessions is empty.
func writeSessionsTable(w io.Writer, sessions []session.Session) error {
	if len(sessions) == 0 {
		_, err := fmt.Fprintln(w, "No saved sessions.")
		return err
	}

	tw := tabwriter.NewWriter(w, 0, 4, 2, ' ', 0)

	if _, err := fmt.Fprintln(tw, "ID\tTITLE\tPROJECT\tAGENT\tUPDATED"); err != nil {
		return err
	}

	for _, s := range sessions {
		if _, err := fmt.Fprintf(tw, "%s\t%s\t%s\t%s\t%s\n",
			s.ID, s.Title, s.Project, s.Agent, s.Updated.Format("2006-01-02 15:04")); err != nil {
			return err
		}
	}

	return tw.Flush()
}

// defaultOpenSessionStore is the production sessionStoreOpener: it loads
// config from disk to derive the current project root, then opens the
// on-disk sqlite session store at its default path.
func defaultOpenSessionStore() (sessionStore, string, error) {
	loaded, err := config.Load()
	if err != nil {
		return nil, "", fmt.Errorf("sessions: load config: %w", err)
	}

	store, err := sessiondb.Open(sessiondb.DefaultPath())
	if err != nil {
		return nil, "", fmt.Errorf("sessions: %w", err)
	}

	return store, loaded.ProjectRoot, nil
}
