package cli

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"golang.org/x/term"

	"github.com/iperez/agens/internal/auth"
)

// loginFunc performs the ChatGPT OAuth device/browser login flow and
// returns the resulting credentials entry. It is a local seam: the real
// implementation lives in internal/auth/chatgpt, a package that does not
// exist yet, so production wiring is completed once that package lands.
type loginFunc func(ctx context.Context, out io.Writer) (auth.Entry, error)

// authDeps is the dependency-injection seam for the `auth` command tree,
// mirroring loopBuilder in chat.go: production code wires real
// implementations, tests inject scripted stand-ins so the tree can be
// exercised without a real OAuth flow, a real terminal, or the real
// credentials file location.
type authDeps struct {
	login      loginFunc
	readSecret func(prompt string, out io.Writer) (string, error)
	authPath   func() string
	now        func() time.Time
}

// errChatGPTLoginNotWired is returned by the production login stand-in
// until the real internal/auth/chatgpt.Login is wired in.
var errChatGPTLoginNotWired = errors.New("auth: chatgpt login not yet wired")

func notWiredLogin(context.Context, io.Writer) (auth.Entry, error) {
	return auth.Entry{}, errChatGPTLoginNotWired
}

func newAuthCommand() *cobra.Command {
	return newAuthCommandWithDeps(authDeps{
		login:      notWiredLogin,
		readSecret: readSecretFromTerminal,
		authPath:   auth.DefaultPath,
		now:        time.Now,
	})
}

func newAuthCommandWithDeps(deps authDeps) *cobra.Command {
	if deps.now == nil {
		deps.now = time.Now
	}

	cmd := &cobra.Command{
		Use:   "auth",
		Short: "Manage provider credentials",
	}

	cmd.AddCommand(newAuthLoginCommand(deps))
	cmd.AddCommand(newAuthStatusCommand(deps))
	cmd.AddCommand(newAuthLogoutCommand(deps))
	return cmd
}

// newAuthLoginCommand builds `auth login`, whose bare form runs the ChatGPT
// OAuth flow via deps.login and persists the resulting entry under the
// "chatgpt" provider id, preserving every other provider already on disk.
func newAuthLoginCommand(deps authDeps) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "login",
		Short: "Authenticate with ChatGPT, or store an API key with the api-key subcommand",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			entry, err := deps.login(cmd.Context(), cmd.OutOrStdout())
			if err != nil {
				return fmt.Errorf("auth login: %w", err)
			}

			if err := saveProviderEntry(deps.authPath(), "chatgpt", entry); err != nil {
				return fmt.Errorf("auth login: %w", err)
			}

			_, err = fmt.Fprintln(cmd.OutOrStdout(), "Logged in to ChatGPT.")
			return err
		},
	}

	cmd.AddCommand(newAuthLoginAPIKeyCommand(deps))
	return cmd
}

func newAuthLoginAPIKeyCommand(deps authDeps) *cobra.Command {
	var apiKeyFlag string

	cmd := &cobra.Command{
		Use:   "api-key <provider>",
		Short: "Store an API key for a provider",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			providerID := args[0]

			key := apiKeyFlag
			if key == "" {
				secret, err := deps.readSecret(fmt.Sprintf("Enter API key for %s: ", providerID), cmd.OutOrStdout())
				if err != nil {
					return fmt.Errorf("auth login api-key: %w", err)
				}
				key = secret
			}
			if key == "" {
				return fmt.Errorf("auth login api-key: empty API key for provider %q", providerID)
			}

			path := deps.authPath()
			file, err := loadAuthFile(path)
			if err != nil {
				return fmt.Errorf("auth login api-key: %w", err)
			}

			entry := file[providerID]
			entry.APIKey = key
			file[providerID] = entry

			if err := auth.Save(path, file); err != nil {
				return fmt.Errorf("auth login api-key: %w", err)
			}

			_, err = fmt.Fprintf(cmd.OutOrStdout(), "Saved API key for %s.\n", providerID)
			return err
		},
	}

	cmd.Flags().StringVar(&apiKeyFlag, "api-key", "", "API key value; bypasses the interactive prompt (for scripting/non-TTY use)")
	return cmd
}

func newAuthStatusCommand(deps authDeps) *cobra.Command {
	return &cobra.Command{
		Use:   "status",
		Short: "Show configured provider credentials",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			file, err := loadAuthFile(deps.authPath())
			if err != nil {
				return fmt.Errorf("auth status: %w", err)
			}

			if len(file) == 0 {
				_, err := fmt.Fprintln(cmd.OutOrStdout(), "No credentials configured.")
				return err
			}

			providerIDs := make([]string, 0, len(file))
			for providerID := range file {
				providerIDs = append(providerIDs, providerID)
			}
			sort.Strings(providerIDs)

			for _, providerID := range providerIDs {
				if _, err := fmt.Fprintln(cmd.OutOrStdout(), formatStatusLine(providerID, file[providerID], deps.now())); err != nil {
					return err
				}
			}
			return nil
		},
	}
}

// formatStatusLine renders one provider's redacted status: authentication
// method, account identifier, and expiry. It never includes entry.APIKey,
// entry.AccessToken, or entry.RefreshToken.
func formatStatusLine(providerID string, entry auth.Entry, now time.Time) string {
	method := "unknown"
	switch {
	case entry.APIKey != "":
		method = "api-key"
	case entry.AccessToken != "":
		method = "oauth"
	}

	account := entry.AccountID
	if account == "" {
		account = "-"
	}

	expiry := "-"
	if entry.ExpiresAt != nil {
		validity := "valid"
		if now.After(*entry.ExpiresAt) {
			validity = "expired"
		}
		expiry = fmt.Sprintf("%s (%s)", entry.ExpiresAt.UTC().Format(time.RFC3339), validity)
	}

	return fmt.Sprintf("%s: method=%s account=%s expires=%s", providerID, method, account, expiry)
}

// newAuthLogoutCommand builds `auth logout <provider>`. Logging out a
// provider with no stored entry is a no-op that reports success: an absent
// entry already satisfies the caller's intent of not being logged in.
func newAuthLogoutCommand(deps authDeps) *cobra.Command {
	return &cobra.Command{
		Use:   "logout <provider>",
		Short: "Remove stored credentials for a provider",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			providerID := args[0]
			path := deps.authPath()

			file, err := loadAuthFile(path)
			if err != nil {
				return fmt.Errorf("auth logout: %w", err)
			}

			if _, ok := file[providerID]; !ok {
				_, err := fmt.Fprintf(cmd.OutOrStdout(), "No credentials stored for %s.\n", providerID)
				return err
			}

			delete(file, providerID)

			if err := auth.Save(path, file); err != nil {
				return fmt.Errorf("auth logout: %w", err)
			}

			_, err = fmt.Fprintf(cmd.OutOrStdout(), "Logged out of %s.\n", providerID)
			return err
		},
	}
}

// loadAuthFile reads the credentials file at path, treating a missing file
// as an empty one so the first-ever login or status check does not require
// the file to pre-exist.
func loadAuthFile(path string) (auth.File, error) {
	file, err := auth.Load(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return auth.File{}, nil
		}
		return nil, err
	}
	return file, nil
}

// saveProviderEntry loads the credentials file at path, replaces the entry
// for providerID, and persists the result, preserving every other provider
// already on disk.
func saveProviderEntry(path, providerID string, entry auth.Entry) error {
	file, err := loadAuthFile(path)
	if err != nil {
		return err
	}
	file[providerID] = entry
	return auth.Save(path, file)
}

// openControllingTTY opens the process's controlling terminal. It is a
// package-level var, following the same testability seam as
// selectPrompter's /dev/tty handling in prompter.go, so tests can force the
// no-controlling-terminal fallback deterministically.
var openControllingTTY = func() (*os.File, error) {
	return os.OpenFile("/dev/tty", os.O_RDWR, 0)
}

// readSecretFromTerminal is the production secret reader for `auth login
// api-key`. When a controlling terminal is available, it prompts on it and
// reads the secret with echo disabled via term.ReadPassword. Otherwise
// (e.g. the process is fully piped, as in CI or scripting), it falls back
// to reading one trimmed line from stdin.
func readSecretFromTerminal(prompt string, out io.Writer) (string, error) {
	tty, err := openControllingTTY()
	if err != nil {
		return readSecretLineFromStdin(prompt, out)
	}
	defer func() { _ = tty.Close() }()

	if _, err := fmt.Fprint(out, prompt); err != nil {
		return "", fmt.Errorf("cli: write secret prompt: %w", err)
	}

	secret, err := term.ReadPassword(int(tty.Fd()))
	if _, werr := fmt.Fprintln(out); werr != nil {
		return "", fmt.Errorf("cli: write secret prompt newline: %w", werr)
	}
	if err != nil {
		return "", fmt.Errorf("cli: read secret: %w", err)
	}
	return strings.TrimSpace(string(secret)), nil
}

func readSecretLineFromStdin(prompt string, out io.Writer) (string, error) {
	if _, err := fmt.Fprint(out, prompt); err != nil {
		return "", fmt.Errorf("cli: write secret prompt: %w", err)
	}

	line, err := bufio.NewReader(os.Stdin).ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return "", fmt.Errorf("cli: read secret from stdin: %w", err)
	}
	return strings.TrimSpace(line), nil
}
