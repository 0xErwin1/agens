package cli

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/auth"
)

func TestAuthCommand_BareLoginInvokesInjectedLogin(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"
	called := false
	deps := authDeps{
		login: func(context.Context, io.Writer) (auth.Entry, error) {
			called = true
			return auth.Entry{AccessToken: "at-x"}, nil
		},
		readSecret: func(string, io.Writer) (string, error) {
			t.Fatal("readSecret should not be called for bare login")
			return "", nil
		},
		authPath: func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"login"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !called {
		t.Fatal("bare `auth login` did not invoke the injected login func")
	}

	file, err := auth.Load(path)
	if err != nil {
		t.Fatalf("auth.Load() error = %v", err)
	}
	if file["openai-chatgpt"].AccessToken != "at-x" {
		t.Fatalf("bare `auth login` did not persist the entry under %q, got file = %+v", "openai-chatgpt", file)
	}
}

// TestDefaultAuthDeps_WiresRealChatGPTLogin verifies the production command
// tree wires chatGPTLogin (the real internal/auth/chatgpt.Login adapter)
// rather than the notWiredLogin stand-in, without invoking it — invoking it
// would open a real browser and hit the real network.
func TestDefaultAuthDeps_WiresRealChatGPTLogin(t *testing.T) {
	deps := defaultAuthDeps()

	got := reflect.ValueOf(deps.login).Pointer()
	want := reflect.ValueOf(chatGPTLogin).Pointer()
	if got != want {
		t.Fatal("defaultAuthDeps().login is not wired to chatGPTLogin")
	}
}

func TestAuthCommand_LoginAPIKeyInvokesInjectedReadSecret(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"
	called := false

	deps := authDeps{
		login: func(context.Context, io.Writer) (auth.Entry, error) {
			t.Fatal("login should not be called for `auth login api-key`")
			return auth.Entry{}, nil
		},
		readSecret: func(prompt string, _ io.Writer) (string, error) {
			called = true
			if !strings.Contains(prompt, "openai-api") {
				t.Fatalf("readSecret prompt = %q, want it to mention the provider", prompt)
			}
			return "sk-injected", nil
		},
		authPath: func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"login", "api-key", "openai-api"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !called {
		t.Fatal("`auth login api-key` did not invoke the injected readSecret")
	}

	file, err := auth.Load(path)
	if err != nil {
		t.Fatalf("auth.Load() error = %v", err)
	}
	if file["openai-api"].APIKey != "sk-injected" {
		t.Fatalf("saved APIKey = %q, want %q", file["openai-api"].APIKey, "sk-injected")
	}
}

func TestAuthCommand_LoginAPIKeyPreservesOtherProviders(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"
	if err := auth.Save(path, auth.File{"anthropic-api": {APIKey: "sk-anthropic"}}); err != nil {
		t.Fatal(err)
	}

	deps := authDeps{
		login: notWiredLogin,
		readSecret: func(string, io.Writer) (string, error) {
			return "sk-new-openai", nil
		},
		authPath: func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"login", "api-key", "openai-api"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	file, err := auth.Load(path)
	if err != nil {
		t.Fatalf("auth.Load() error = %v", err)
	}
	if file["anthropic-api"].APIKey != "sk-anthropic" {
		t.Fatalf("existing provider entry lost, got %+v", file["anthropic-api"])
	}
	if file["openai-api"].APIKey != "sk-new-openai" {
		t.Fatalf("new provider entry = %+v, want APIKey %q", file["openai-api"], "sk-new-openai")
	}
}

func TestAuthCommand_LoginAPIKeyFlagBypassesPrompt(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"

	deps := authDeps{
		login: notWiredLogin,
		readSecret: func(string, io.Writer) (string, error) {
			t.Fatal("readSecret should not be called when --api-key is provided")
			return "", nil
		},
		authPath: func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"login", "api-key", "openai-api", "--api-key", "sk-flag"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	file, err := auth.Load(path)
	if err != nil {
		t.Fatalf("auth.Load() error = %v", err)
	}
	if file["openai-api"].APIKey != "sk-flag" {
		t.Fatalf("saved APIKey = %q, want %q", file["openai-api"].APIKey, "sk-flag")
	}
}

func TestAuthCommand_StatusHidesSecretsAndPrintsMetadata(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"
	expiresAt := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	if err := auth.Save(path, auth.File{
		"openai-api": {APIKey: "sk-super-secret", AccountID: "acct-1"},
		"chatgpt":    {AccessToken: "at-super-secret", RefreshToken: "rt-super-secret", ExpiresAt: &expiresAt},
	}); err != nil {
		t.Fatal(err)
	}

	deps := authDeps{
		login:      notWiredLogin,
		readSecret: failingReadSecret(t),
		authPath:   func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"status"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	got := out.String()
	for _, secret := range []string{"sk-super-secret", "at-super-secret", "rt-super-secret"} {
		if strings.Contains(got, secret) {
			t.Fatalf("status output leaked a secret value: %q\noutput:\n%s", secret, got)
		}
	}
	if !strings.Contains(got, "openai-api") || !strings.Contains(got, "acct-1") {
		t.Fatalf("status output = %q, want it to mention provider id and account", got)
	}
	if !strings.Contains(got, "chatgpt") || !strings.Contains(got, "2026-01-02") {
		t.Fatalf("status output = %q, want it to mention provider id and expiry", got)
	}
}

func TestAuthCommand_StatusWithNoCredentials(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"

	deps := authDeps{
		login:      notWiredLogin,
		readSecret: failingReadSecret(t),
		authPath:   func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"status"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if strings.TrimSpace(out.String()) == "" {
		t.Fatal("status output = empty, want a message when no credentials are configured")
	}
}

func TestAuthCommand_LogoutRemovesOnlyTargetProvider(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"
	if err := auth.Save(path, auth.File{
		"openai-api":    {APIKey: "sk-openai"},
		"anthropic-api": {APIKey: "sk-anthropic"},
	}); err != nil {
		t.Fatal(err)
	}

	deps := authDeps{
		login:      notWiredLogin,
		readSecret: failingReadSecret(t),
		authPath:   func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"logout", "openai-api"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	file, err := auth.Load(path)
	if err != nil {
		t.Fatalf("auth.Load() error = %v", err)
	}
	if _, ok := file["openai-api"]; ok {
		t.Fatal("logout did not remove the target provider entry")
	}
	if file["anthropic-api"].APIKey != "sk-anthropic" {
		t.Fatalf("logout removed an unrelated provider entry, got %+v", file)
	}
}

func TestAuthCommand_LogoutOfAbsentProviderIsIdempotent(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/auth.json"
	if err := auth.Save(path, auth.File{"anthropic-api": {APIKey: "sk-anthropic"}}); err != nil {
		t.Fatal(err)
	}

	deps := authDeps{
		login:      notWiredLogin,
		readSecret: failingReadSecret(t),
		authPath:   func() string { return path },
	}

	cmd := newAuthCommandWithDeps(deps)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"logout", "openai-api"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil for logging out an already-absent provider", err)
	}
	if strings.TrimSpace(out.String()) == "" {
		t.Fatal("logout of an absent provider produced no message")
	}

	file, err := auth.Load(path)
	if err != nil {
		t.Fatalf("auth.Load() error = %v", err)
	}
	if file["anthropic-api"].APIKey != "sk-anthropic" {
		t.Fatalf("idempotent logout mutated unrelated provider entries, got %+v", file)
	}
}

func TestReadSecretFromTerminal_NoTTYFallsBackToStdinLine(t *testing.T) {
	restore := forceNoControllingTTY(t)
	defer restore()

	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := w.WriteString("piped-secret\n"); err != nil {
		t.Fatal(err)
	}
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}

	origStdin := os.Stdin
	os.Stdin = r
	defer func() { os.Stdin = origStdin }()

	out := new(bytes.Buffer)
	got, err := readSecretFromTerminal("Enter secret: ", out)
	if err != nil {
		t.Fatalf("readSecretFromTerminal() error = %v, want nil", err)
	}
	if got != "piped-secret" {
		t.Fatalf("readSecretFromTerminal() = %q, want %q", got, "piped-secret")
	}
}

func forceNoControllingTTY(t *testing.T) func() {
	t.Helper()
	original := openControllingTTY
	openControllingTTY = func() (*os.File, error) {
		return nil, errors.New("no controlling terminal in test")
	}
	return func() { openControllingTTY = original }
}

func failingReadSecret(t *testing.T) func(string, io.Writer) (string, error) {
	t.Helper()
	return func(string, io.Writer) (string, error) {
		t.Fatal("readSecret should not be called")
		return "", nil
	}
}
