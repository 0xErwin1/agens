package chatgpt

import (
	"context"
	"crypto/subtle"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"sync"
	"time"

	"github.com/0xErwin1/agens/internal/auth"
)

// loopbackPorts are the ports tried in order when starting the local OAuth
// callback server, matching the ports the openai/codex reference client
// registers as valid redirect_uri targets with OpenAI's authorize
// endpoint.
var loopbackPorts = []int{1455, 1457}

// authorizeEndpoint and tokenEndpoint are package-level vars defaulting to
// the grounded OAuth endpoint consts. Tests override them to redirect
// Login at an httptest.Server without touching the grounded consts or
// hitting the real network, mirroring the browserCommand seam in
// browser.go.
var (
	authorizeEndpoint = authorizeURL
	tokenEndpoint     = tokenURL
)

// shutdownGrace bounds how long Login waits for the loopback server to
// release its port once the OAuth callback has been resolved or ctx ends.
const shutdownGrace = 5 * time.Second

// LoginOptions configures Login's browser interaction and HTTP transport.
type LoginOptions struct {
	// Out receives the authorize URL and login progress output. Defaults
	// to os.Stderr when nil.
	Out io.Writer

	// OpenBrowser opens url in the user's browser. Defaults to
	// openBrowser(Out, url) when nil.
	OpenBrowser func(url string) error

	// HTTPClient performs the token exchange request. Defaults to
	// http.DefaultClient when nil.
	HTTPClient *http.Client
}

// portError reports that Login could not bind any of the candidate
// loopback ports for the OAuth callback server. It carries no secret,
// only the ports that were attempted.
type portError struct {
	ports []int
}

func (e *portError) Error() string {
	return fmt.Sprintf("chatgpt: could not bind a loopback callback port (tried %v): all in use", e.ports)
}

// stateMismatchError reports that the OAuth callback's state parameter did
// not match the state Login generated for this login attempt, indicating
// a possible CSRF attempt or a stale/duplicate callback request.
type stateMismatchError struct{}

func (e *stateMismatchError) Error() string {
	return "chatgpt: OAuth callback state mismatch"
}

// callbackError reports that the OAuth provider's callback did not carry
// a usable authorization code, either because it reported an error or
// omitted the code parameter entirely.
type callbackError struct {
	reason string
}

func (e *callbackError) Error() string {
	return fmt.Sprintf("chatgpt: OAuth callback error: %s", e.reason)
}

// callbackResult carries the single outcome the loopback server's handler
// delivers to Login: either a usable authorization code, or the error
// that prevented one.
type callbackResult struct {
	code string
	err  error
}

// Login runs the ChatGPT PKCE browser login flow: it binds a local
// loopback HTTP server, opens the OpenAI authorize URL in the user's
// browser, waits for the single OAuth callback or ctx to end, exchanges a
// returned authorization code for tokens, and returns the resulting
// auth.Entry. Login never persists the entry; the caller is responsible
// for storing it under the appropriate provider key.
func Login(ctx context.Context, opts LoginOptions) (auth.Entry, error) {
	out := opts.Out
	if out == nil {
		out = os.Stderr
	}
	httpClient := opts.HTTPClient
	if httpClient == nil {
		httpClient = http.DefaultClient
	}
	openBrowserFn := opts.OpenBrowser
	if openBrowserFn == nil {
		openBrowserFn = func(authURL string) error { return openBrowser(out, authURL) }
	}

	verifierChallenge, err := generatePKCE()
	if err != nil {
		return auth.Entry{}, err
	}
	state, err := generateState()
	if err != nil {
		return auth.Entry{}, err
	}

	listener, port, err := bindLoopback(loopbackPorts)
	if err != nil {
		return auth.Entry{}, err
	}

	redirectURI := fmt.Sprintf(redirectURITemplate, port)
	authURL := buildAuthorizeURL(verifierChallenge.challenge, state, redirectURI)

	results := make(chan callbackResult, 1)
	srv := &http.Server{Handler: newCallbackHandler(state, results)}
	go func() { _ = srv.Serve(listener) }()
	defer shutdownLoopback(srv)

	if err := openBrowserFn(authURL); err != nil {
		return auth.Entry{}, err
	}

	select {
	case <-ctx.Done():
		return auth.Entry{}, ctx.Err()
	case result := <-results:
		if result.err != nil {
			return auth.Entry{}, result.err
		}
		return exchangeCode(ctx, httpClient, tokenEndpoint, result.code, redirectURI, verifierChallenge.verifier, time.Now)
	}
}

// bindLoopback tries each of ports in order and returns a listener bound
// to the first one that succeeds. If none can be bound, it returns a
// *portError naming every port that was attempted.
func bindLoopback(ports []int) (net.Listener, int, error) {
	for _, port := range ports {
		listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", port))
		if err == nil {
			return listener, port, nil
		}
	}
	return nil, 0, &portError{ports: ports}
}

// shutdownLoopback stops srv within shutdownGrace so the bound loopback
// port is released even if the caller's ctx has already ended.
func shutdownLoopback(srv *http.Server) {
	ctx, cancel := context.WithTimeout(context.Background(), shutdownGrace)
	defer cancel()
	_ = srv.Shutdown(ctx)
}

// buildAuthorizeURL assembles the OpenAI OAuth authorize URL for a single
// login attempt from the grounded client configuration plus this
// attempt's PKCE challenge, state, and loopback redirect_uri.
func buildAuthorizeURL(codeChallenge, state, redirectURI string) string {
	values := url.Values{}
	for key, value := range authorizeExtraParams {
		values.Set(key, value)
	}
	values.Set("client_id", clientID)
	values.Set("redirect_uri", redirectURI)
	values.Set("scope", scope)
	values.Set("code_challenge", codeChallenge)
	values.Set("state", state)

	return authorizeEndpoint + "?" + values.Encode()
}

// newCallbackHandler returns the single-shot /auth/callback handler for
// one login attempt. It delivers exactly one callbackResult to results
// (via sync.Once, so retried or duplicate requests are ignored after the
// first), comparing the callback's state against wantState in constant
// time to defeat CSRF/timing attacks against the comparison itself.
func newCallbackHandler(wantState string, results chan<- callbackResult) http.HandlerFunc {
	var once sync.Once
	deliver := func(result callbackResult) {
		once.Do(func() { results <- result })
	}

	return func(w http.ResponseWriter, r *http.Request) {
		query := r.URL.Query()

		if reason := query.Get("error"); reason != "" {
			deliver(callbackResult{err: &callbackError{reason: reason}})
			writeCallbackPage(w, false)
			return
		}

		gotState := query.Get("state")
		if subtle.ConstantTimeCompare([]byte(gotState), []byte(wantState)) != 1 {
			deliver(callbackResult{err: &stateMismatchError{}})
			writeCallbackPage(w, false)
			return
		}

		code := query.Get("code")
		if code == "" {
			deliver(callbackResult{err: &callbackError{reason: "missing code parameter"}})
			writeCallbackPage(w, false)
			return
		}

		deliver(callbackResult{code: code})
		writeCallbackPage(w, true)
	}
}

// writeCallbackPage renders the minimal page shown in the user's browser
// once the OAuth callback has been handled, so they know it is safe to
// return to the terminal.
func writeCallbackPage(w http.ResponseWriter, success bool) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	if success {
		_, _ = io.WriteString(w, "<html><body><h1>Login successful</h1><p>You can close this window and return to your terminal.</p></body></html>")
		return
	}
	w.WriteHeader(http.StatusBadRequest)
	_, _ = io.WriteString(w, "<html><body><h1>Login failed</h1><p>You can close this window and return to your terminal.</p></body></html>")
}
