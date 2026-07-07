package webfetch

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/tool"
)

func TestNew(t *testing.T) {
	w := New()
	if w == nil {
		t.Fatal("New() = nil, want non-nil")
	}
}

func TestWebFetch_Name(t *testing.T) {
	w := New()
	if got := w.Name(); got != "webfetch" {
		t.Fatalf("Name() = %q, want %q", got, "webfetch")
	}
}

func TestWebFetch_Schema(t *testing.T) {
	w := New()
	schema := w.Schema()
	if schema == nil {
		t.Fatal("Schema() = nil, want non-nil")
	}
	if schema.Type != "object" {
		t.Fatalf("Schema().Type = %q, want %q", schema.Type, "object")
	}
	if _, ok := schema.Properties["url"]; !ok {
		t.Fatalf("Schema().Properties missing %q", "url")
	}
	if _, ok := schema.Properties["timeout_seconds"]; !ok {
		t.Fatalf("Schema().Properties missing %q", "timeout_seconds")
	}

	required := map[string]bool{}
	for _, r := range schema.Required {
		required[r] = true
	}
	if !required["url"] {
		t.Fatalf("Schema().Required = %v, want it to include %q", schema.Required, "url")
	}
	if required["timeout_seconds"] {
		t.Fatalf("Schema().Required = %v, want %q to be optional", schema.Required, "timeout_seconds")
	}
}

func TestReadCapped(t *testing.T) {
	t.Run("under limit is intact and not truncated", func(t *testing.T) {
		data, truncated, err := readCapped(strings.NewReader("hello"), 100)
		if err != nil {
			t.Fatalf("readCapped() error = %v, want nil", err)
		}
		if truncated {
			t.Fatal("readCapped() truncated = true, want false")
		}
		if string(data) != "hello" {
			t.Fatalf("readCapped() data = %q, want %q", data, "hello")
		}
	})

	t.Run("exactly at limit is intact and not truncated", func(t *testing.T) {
		data, truncated, err := readCapped(strings.NewReader("hello"), 5)
		if err != nil {
			t.Fatalf("readCapped() error = %v, want nil", err)
		}
		if truncated {
			t.Fatal("readCapped() truncated = true, want false")
		}
		if string(data) != "hello" {
			t.Fatalf("readCapped() data = %q, want %q", data, "hello")
		}
	})

	t.Run("over limit is trimmed and truncated", func(t *testing.T) {
		data, truncated, err := readCapped(strings.NewReader("hello world"), 5)
		if err != nil {
			t.Fatalf("readCapped() error = %v, want nil", err)
		}
		if !truncated {
			t.Fatal("readCapped() truncated = false, want true")
		}
		if string(data) != "hello" {
			t.Fatalf("readCapped() data = %q, want %q", data, "hello")
		}
	})

	t.Run("reader error propagates", func(t *testing.T) {
		wantErr := errors.New("boom")
		_, _, err := readCapped(errReader{err: wantErr}, 100)
		if !errors.Is(err, wantErr) {
			t.Fatalf("readCapped() error = %v, want %v", err, wantErr)
		}
	})
}

type errReader struct{ err error }

func (r errReader) Read([]byte) (int, error) { return 0, r.err }

var _ io.Reader = errReader{}

func TestMapFetchError(t *testing.T) {
	t.Run("turn ctx cancelled takes priority", func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		cancel()

		res, err := mapFetchError(ctx, context.DeadlineExceeded, time.Second)
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("mapFetchError() error = %v, want context.Canceled", err)
		}
		if res.Text != "" || res.IsError {
			t.Fatalf("mapFetchError() Result = %+v, want zero value", res)
		}
	})

	t.Run("deadline exceeded reports a timeout", func(t *testing.T) {
		res, err := mapFetchError(context.Background(), context.DeadlineExceeded, 30*time.Second)
		if err != nil {
			t.Fatalf("mapFetchError() error = %v, want nil", err)
		}
		if !res.IsError {
			t.Fatal("mapFetchError() IsError = false, want true")
		}
		if !strings.Contains(res.Text, "timed out") || !strings.Contains(res.Text, "30s") {
			t.Fatalf("mapFetchError() Text = %q, want it to mention the timeout and duration", res.Text)
		}
	})

	t.Run("host not allowed is reported without leaking the address", func(t *testing.T) {
		res, err := mapFetchError(context.Background(), errHostNotAllowed, 30*time.Second)
		if err != nil {
			t.Fatalf("mapFetchError() error = %v, want nil", err)
		}
		if !res.IsError {
			t.Fatal("mapFetchError() IsError = false, want true")
		}
		if !strings.Contains(res.Text, "host not allowed") {
			t.Fatalf("mapFetchError() Text = %q, want it to mention 'host not allowed'", res.Text)
		}
		if strings.ContainsAny(res.Text, "0123456789") {
			t.Fatalf("mapFetchError() Text = %q, want it to not leak a resolved IP", res.Text)
		}
	})

	t.Run("generic error falls through", func(t *testing.T) {
		wantErr := errors.New("connection refused")
		res, err := mapFetchError(context.Background(), wantErr, 30*time.Second)
		if err != nil {
			t.Fatalf("mapFetchError() error = %v, want nil", err)
		}
		if !res.IsError {
			t.Fatal("mapFetchError() IsError = false, want true")
		}
		if !strings.Contains(res.Text, "request failed") || !strings.Contains(res.Text, "connection refused") {
			t.Fatalf("mapFetchError() Text = %q, want it to mention the failure and the underlying error", res.Text)
		}
	})
}

// jsonInput marshals fields into a json.RawMessage suitable as Execute's
// input argument.
func jsonInput(t *testing.T, fields map[string]any) json.RawMessage {
	t.Helper()
	data, err := json.Marshal(fields)
	if err != nil {
		t.Fatalf("marshal input: %v", err)
	}
	return json.RawMessage(data)
}

// failIfHit fails the test if the returned handler is ever invoked, for
// asserting that a rejected request never reaches the network.
func failIfHit(t *testing.T) http.HandlerFunc {
	t.Helper()
	return func(http.ResponseWriter, *http.Request) {
		t.Fatal("server was contacted, want no network call")
	}
}

func TestWebFetch_Execute_InvalidInput(t *testing.T) {
	tests := []struct {
		name  string
		input json.RawMessage
	}{
		{name: "malformed JSON", input: json.RawMessage(`{"url":`)},
		{name: "empty url", input: jsonInput(t, map[string]any{"url": ""})},
		{name: "whitespace url", input: jsonInput(t, map[string]any{"url": "   "})},
		{name: "negative timeout", input: jsonInput(t, map[string]any{"url": "https://example.com", "timeout_seconds": -1})},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			srv := httptest.NewServer(failIfHit(t))
			defer srv.Close()

			w := New()
			res, err := w.Execute(context.Background(), tc.input)
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil", err)
			}
			if !res.IsError {
				t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
			}
			if !strings.Contains(res.Text, "webfetch: invalid input") {
				t.Fatalf("Execute() Text = %q, want it to mention invalid input", res.Text)
			}
		})
	}
}

func TestWebFetch_Execute_SchemeRejection(t *testing.T) {
	tests := []struct {
		name string
		url  string
	}{
		{name: "file scheme", url: "file:///etc/passwd"},
		{name: "ftp scheme", url: "ftp://host/x"},
		{name: "data scheme", url: "data:text/plain,x"},
		{name: "unparsable", url: "://bad"},
		{name: "empty host", url: "https:///path"},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			w := New()
			res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": tc.url}))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil", err)
			}
			if !res.IsError {
				t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
			}
			if !strings.Contains(res.Text, "only http and https URLs are supported") {
				t.Fatalf("Execute() Text = %q, want it to mention scheme restriction", res.Text)
			}
		})
	}
}

func TestWebFetch_Execute_HappyPathText(t *testing.T) {
	var receivedHeader http.Header
	srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
		receivedHeader = r.Header.Clone()
		rw.Header().Set("Content-Type", "text/plain")
		_, _ = rw.Write([]byte("hello from server"))
	}))
	defer srv.Close()

	w := New()
	res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "hello from server") {
		t.Fatalf("Execute() Text = %q, want it to contain the body", res.Text)
	}

	if got := receivedHeader.Get("User-Agent"); got != userAgent {
		t.Fatalf("received User-Agent = %q, want %q", got, userAgent)
	}
	if got := receivedHeader.Get("Authorization"); got != "" {
		t.Fatalf("received Authorization = %q, want empty (no credential leakage)", got)
	}
	for name := range receivedHeader {
		if strings.Contains(strings.ToLower(name), "api-key") {
			t.Fatalf("received header %q, want no api-key header", name)
		}
	}
}

func TestWebFetch_Execute_HTMLExtraction(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
		rw.Header().Set("Content-Type", "text/html")
		_, _ = rw.Write([]byte(`<html><head><style>body{color:red}</style></head>` +
			`<body><script>alert(1)</script><p>Visible text</p></body></html>`))
	}))
	defer srv.Close()

	w := New()
	res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "Visible text") {
		t.Fatalf("Execute() Text = %q, want it to contain the visible text", res.Text)
	}
	if strings.Contains(res.Text, "color:red") || strings.Contains(res.Text, "alert(1)") {
		t.Fatalf("Execute() Text = %q, want script/style content excluded", res.Text)
	}
	if strings.Contains(res.Text, "<p>") {
		t.Fatalf("Execute() Text = %q, want raw tags excluded", res.Text)
	}
}

func TestWebFetch_Execute_NonHTMLPassthrough(t *testing.T) {
	const body = `{"key":"value","tag":"<not-html>"}`
	srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
		rw.Header().Set("Content-Type", "application/json")
		_, _ = rw.Write([]byte(body))
	}))
	defer srv.Close()

	w := New()
	res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if res.Text != body {
		t.Fatalf("Execute() Text = %q, want raw body %q verbatim", res.Text, body)
	}
}

func TestWebFetch_Execute_OutputCap(t *testing.T) {
	t.Run("over cap is truncated with a notice", func(t *testing.T) {
		big := strings.Repeat("a", maxBodyBytes+1024)
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			rw.Header().Set("Content-Type", "text/plain")
			_, _ = rw.Write([]byte(big))
		}))
		defer srv.Close()

		w := New()
		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil", err)
		}
		if res.IsError {
			t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
		}
		if !strings.Contains(res.Text, "[response truncated after 100 KiB]") {
			t.Fatalf("Execute() Text does not contain the truncation notice (len=%d)", len(res.Text))
		}
	})

	t.Run("under cap is intact", func(t *testing.T) {
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			rw.Header().Set("Content-Type", "text/plain")
			_, _ = rw.Write([]byte("short body"))
		}))
		defer srv.Close()

		w := New()
		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil", err)
		}
		if res.Text != "short body" {
			t.Fatalf("Execute() Text = %q, want %q unmodified", res.Text, "short body")
		}
	})

	t.Run("empty 2xx body reports no content", func(t *testing.T) {
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			rw.WriteHeader(http.StatusOK)
		}))
		defer srv.Close()

		w := New()
		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil", err)
		}
		if res.IsError {
			t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
		}
		if res.Text != "(no content; 200 OK)" {
			t.Fatalf("Execute() Text = %q, want %q", res.Text, "(no content; 200 OK)")
		}
	})
}

func TestWebFetch_Execute_NonSuccessStatus(t *testing.T) {
	tests := []struct {
		name   string
		status int
		body   string
	}{
		{name: "404", status: http.StatusNotFound, body: "not found here"},
		{name: "500", status: http.StatusInternalServerError, body: "internal failure"},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
				rw.WriteHeader(tc.status)
				_, _ = rw.Write([]byte(tc.body))
			}))
			defer srv.Close()

			w := New()
			res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil", err)
			}
			if !res.IsError {
				t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
			}
			if !strings.Contains(res.Text, http.StatusText(tc.status)) {
				t.Fatalf("Execute() Text = %q, want it to mention the status", res.Text)
			}
			if !strings.Contains(res.Text, tc.body) {
				t.Fatalf("Execute() Text = %q, want it to contain the error body snippet", res.Text)
			}
		})
	}

	t.Run("error body snippet is capped", func(t *testing.T) {
		big := strings.Repeat("e", maxErrorBodyBytes+1024)
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			rw.WriteHeader(http.StatusInternalServerError)
			_, _ = rw.Write([]byte(big))
		}))
		defer srv.Close()

		w := New()
		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil", err)
		}
		if !res.IsError {
			t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
		}
		if len(res.Text) > maxErrorBodyBytes+256 {
			t.Fatalf("Execute() Text length = %d, want it capped near maxErrorBodyBytes", len(res.Text))
		}
	})
}

func TestWebFetch_Execute_TimeoutVsCancel(t *testing.T) {
	t.Run("self-inflicted timeout does not abort the turn", func(t *testing.T) {
		sleeping := make(chan struct{})
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			<-sleeping
		}))
		defer srv.Close()
		defer close(sleeping)

		w := New()
		w.defaultTimeout = 100 * time.Millisecond

		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil (a timeout must not abort the turn)", err)
		}
		if !res.IsError {
			t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
		}
		if !strings.Contains(res.Text, "timed out") {
			t.Fatalf("Execute() Text = %q, want it to mention a timeout", res.Text)
		}
	})

	t.Run("turn cancellation aborts with a Go error", func(t *testing.T) {
		started := make(chan struct{})
		sleeping := make(chan struct{})
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			close(started)
			<-sleeping
		}))
		defer srv.Close()
		defer close(sleeping)

		w := New()

		ctx, cancel := context.WithCancel(context.Background())

		type execResult struct {
			res tool.Result
			err error
		}
		done := make(chan execResult, 1)
		go func() {
			res, err := w.Execute(ctx, jsonInput(t, map[string]any{"url": srv.URL}))
			done <- execResult{res, err}
		}()

		<-started
		cancel()

		select {
		case r := <-done:
			if !errors.Is(r.err, context.Canceled) {
				t.Fatalf("Execute() error = %v, want errors.Is(err, context.Canceled)", r.err)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("Execute() did not return within 5s of cancellation")
		}
	})
}

func TestWebFetch_Execute_SSRF(t *testing.T) {
	t.Run("direct fetch of a metadata address is blocked", func(t *testing.T) {
		w := New()
		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": "http://169.254.169.254/"}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil", err)
		}
		if !res.IsError {
			t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
		}
		if !strings.Contains(res.Text, "host not allowed") {
			t.Fatalf("Execute() Text = %q, want it to mention 'host not allowed'", res.Text)
		}
		if strings.Contains(res.Text, "169.254.169.254") {
			t.Fatalf("Execute() Text = %q, want it to NOT leak the resolved IP", res.Text)
		}
	})

	t.Run("redirect to a metadata address is blocked", func(t *testing.T) {
		srv := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
			http.Redirect(rw, r, "http://169.254.169.254/latest/meta-data/", http.StatusFound)
		}))
		defer srv.Close()

		w := New()
		res, err := w.Execute(context.Background(), jsonInput(t, map[string]any{"url": srv.URL}))
		if err != nil {
			t.Fatalf("Execute() error = %v, want nil", err)
		}
		if !res.IsError {
			t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
		}
		if !strings.Contains(res.Text, "host not allowed") {
			t.Fatalf("Execute() Text = %q, want it to mention 'host not allowed'", res.Text)
		}
		if strings.Contains(res.Text, "meta-data") || strings.Contains(res.Text, "169.254.169.254") {
			t.Fatalf("Execute() Text = %q, want no metadata content leaked", res.Text)
		}
	})
}
