package chatgpt

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/iperez/agens/internal/auth"
)

// fakeTokenServer returns an httptest.Server that answers the token
// exchange with a fixed successful response for accountID, and a pointer
// to a call counter so tests can assert whether the token endpoint was
// ever hit.
func fakeTokenServer(t *testing.T, accountID string) (*httptest.Server, *int32) {
	t.Helper()
	var calls int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&calls, 1)
		_, _ = w.Write([]byte(`{"access_token":"at-login","refresh_token":"rt-login","id_token":"` + fakeIDToken(t, accountID) + `","expires_in":3600}`))
	}))
	return server, &calls
}

// stubTokenEndpoint points the package-level tokenEndpoint seam at server
// for the duration of the current test.
func stubTokenEndpoint(t *testing.T, server *httptest.Server) {
	t.Helper()
	original := tokenEndpoint
	tokenEndpoint = server.URL
	t.Cleanup(func() { tokenEndpoint = original })
}

// parseAuthorizeURL extracts redirect_uri and state from an authorize URL
// built by Login, so tests can drive the loopback callback exactly as a
// real browser redirect would.
func parseAuthorizeURL(t *testing.T, rawURL string) (redirectURI, state string) {
	t.Helper()
	parsed, err := url.Parse(rawURL)
	if err != nil {
		t.Fatalf("authorize URL %q is not parseable: %v", rawURL, err)
	}
	query := parsed.Query()
	redirectURI = query.Get("redirect_uri")
	state = query.Get("state")
	if redirectURI == "" || state == "" {
		t.Fatalf("authorize URL %q missing redirect_uri or state", rawURL)
	}
	return redirectURI, state
}

// callbackGet issues a GET against the loopback callback URL built from
// redirectURI plus the given query values, and returns the response
// status code.
func callbackGet(t *testing.T, redirectURI string, query url.Values) int {
	t.Helper()
	resp, err := http.Get(redirectURI + "?" + query.Encode())
	if err != nil {
		t.Fatalf("callback GET %s error = %v", redirectURI, err)
	}
	defer func() { _ = resp.Body.Close() }()
	return resp.StatusCode
}

func TestLogin_HappyPathBindsFirstPort(t *testing.T) {
	tokenServer, calls := fakeTokenServer(t, "acct-login")
	defer tokenServer.Close()
	stubTokenEndpoint(t, tokenServer)

	var gotRedirectURI string
	opts := LoginOptions{
		OpenBrowser: func(authURL string) error {
			redirectURI, state := parseAuthorizeURL(t, authURL)
			gotRedirectURI = redirectURI
			status := callbackGet(t, redirectURI, url.Values{"code": {"auth-code-1"}, "state": {state}})
			if status != http.StatusOK {
				t.Fatalf("callback status = %d, want %d", status, http.StatusOK)
			}
			return nil
		},
		HTTPClient: tokenServer.Client(),
	}

	entry, err := Login(context.Background(), opts)
	if err != nil {
		t.Fatalf("Login() error = %v", err)
	}
	if entry.AccessToken != "at-login" {
		t.Fatalf("AccessToken = %q, want %q", entry.AccessToken, "at-login")
	}
	if entry.AccountID != "acct-login" {
		t.Fatalf("AccountID = %q, want %q", entry.AccountID, "acct-login")
	}
	if !strings.Contains(gotRedirectURI, ":1455/auth/callback") {
		t.Fatalf("redirect_uri = %q, want it to bind port 1455", gotRedirectURI)
	}
	if atomic.LoadInt32(calls) != 1 {
		t.Fatalf("token endpoint hit %d times, want 1", atomic.LoadInt32(calls))
	}
}

func TestLogin_FallsBackToSecondPortWhenFirstBusy(t *testing.T) {
	blocker, err := net.Listen("tcp", "127.0.0.1:1455")
	if err != nil {
		t.Fatalf("failed to pre-bind 127.0.0.1:1455 for the test: %v", err)
	}
	defer func() { _ = blocker.Close() }()

	tokenServer, _ := fakeTokenServer(t, "acct-fallback")
	defer tokenServer.Close()
	stubTokenEndpoint(t, tokenServer)

	var gotRedirectURI string
	opts := LoginOptions{
		OpenBrowser: func(authURL string) error {
			redirectURI, state := parseAuthorizeURL(t, authURL)
			gotRedirectURI = redirectURI
			callbackGet(t, redirectURI, url.Values{"code": {"auth-code-2"}, "state": {state}})
			return nil
		},
		HTTPClient: tokenServer.Client(),
	}

	entry, err := Login(context.Background(), opts)
	if err != nil {
		t.Fatalf("Login() error = %v", err)
	}
	if entry.AccessToken != "at-login" {
		t.Fatalf("AccessToken = %q, want %q", entry.AccessToken, "at-login")
	}
	if !strings.Contains(gotRedirectURI, ":1457/auth/callback") {
		t.Fatalf("redirect_uri = %q, want it to fall back to port 1457", gotRedirectURI)
	}
}

func TestLogin_BothPortsBusyReturnsTypedError(t *testing.T) {
	first, err := net.Listen("tcp", "127.0.0.1:1455")
	if err != nil {
		t.Fatalf("failed to pre-bind 127.0.0.1:1455 for the test: %v", err)
	}
	defer func() { _ = first.Close() }()

	second, err := net.Listen("tcp", "127.0.0.1:1457")
	if err != nil {
		t.Fatalf("failed to pre-bind 127.0.0.1:1457 for the test: %v", err)
	}
	defer func() { _ = second.Close() }()

	browserCalled := false
	opts := LoginOptions{
		OpenBrowser: func(authURL string) error {
			browserCalled = true
			return nil
		},
	}

	entry, err := Login(context.Background(), opts)
	if err == nil {
		t.Fatal("Login() error = nil, want a port error")
	}
	var perr *portError
	if !errors.As(err, &perr) {
		t.Fatalf("Login() error type = %T, want *portError", err)
	}
	if browserCalled {
		t.Fatal("Login() opened the browser despite both loopback ports being busy")
	}
	if entry != (auth.Entry{}) {
		t.Fatalf("Login() entry = %+v, want zero value", entry)
	}
}

func TestLogin_StateMismatchReturnsTypedErrorNoPersist(t *testing.T) {
	tokenServer, calls := fakeTokenServer(t, "acct-mismatch")
	defer tokenServer.Close()
	stubTokenEndpoint(t, tokenServer)

	opts := LoginOptions{
		OpenBrowser: func(authURL string) error {
			redirectURI, _ := parseAuthorizeURL(t, authURL)
			status := callbackGet(t, redirectURI, url.Values{"code": {"auth-code-3"}, "state": {"wrong-state"}})
			if status != http.StatusBadRequest {
				t.Fatalf("callback status = %d, want %d", status, http.StatusBadRequest)
			}
			return nil
		},
	}

	entry, err := Login(context.Background(), opts)
	if err == nil {
		t.Fatal("Login() error = nil, want a state mismatch error")
	}
	var serr *stateMismatchError
	if !errors.As(err, &serr) {
		t.Fatalf("Login() error type = %T, want *stateMismatchError", err)
	}
	if entry != (auth.Entry{}) {
		t.Fatalf("Login() entry = %+v, want zero value", entry)
	}
	if atomic.LoadInt32(calls) != 0 {
		t.Fatalf("token endpoint hit %d times, want 0 after a state mismatch", atomic.LoadInt32(calls))
	}
}

func TestLogin_CallbackErrorParamReturnsTypedError(t *testing.T) {
	tokenServer, calls := fakeTokenServer(t, "acct-error")
	defer tokenServer.Close()
	stubTokenEndpoint(t, tokenServer)

	opts := LoginOptions{
		OpenBrowser: func(authURL string) error {
			redirectURI, state := parseAuthorizeURL(t, authURL)
			callbackGet(t, redirectURI, url.Values{"error": {"access_denied"}, "state": {state}})
			return nil
		},
	}

	entry, err := Login(context.Background(), opts)
	if err == nil {
		t.Fatal("Login() error = nil, want a callback error")
	}
	var cerr *callbackError
	if !errors.As(err, &cerr) {
		t.Fatalf("Login() error type = %T, want *callbackError", err)
	}
	if entry != (auth.Entry{}) {
		t.Fatalf("Login() entry = %+v, want zero value", entry)
	}
	if atomic.LoadInt32(calls) != 0 {
		t.Fatalf("token endpoint hit %d times, want 0 after a callback error", atomic.LoadInt32(calls))
	}
}

func TestLogin_ContextDeadlineBeforeCallbackReturnsContextError(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()

	opts := LoginOptions{
		OpenBrowser: func(authURL string) error {
			return nil
		},
	}

	entry, err := Login(ctx, opts)
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Login() error = %v, want context.DeadlineExceeded", err)
	}
	if entry != (auth.Entry{}) {
		t.Fatalf("Login() entry = %+v, want zero value", entry)
	}
}
