// Package webfetch implements the "webfetch" tool: it issues a single HTTP
// GET for a model-supplied URL, extracts readable text from HTML responses,
// and caps the amount of data read. It has no dependency on
// internal/agentloop, internal/agent, or internal/cli.
package webfetch

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/0xErwin1/agens/internal/tool"
	"github.com/google/jsonschema-go/jsonschema"
)

const (
	defaultTimeout    = 30 * time.Second
	maxBodyBytes      = 100 << 10
	maxErrorBodyBytes = 4 << 10
	userAgent         = "agens-webfetch/1.0"
)

// WebFetch implements the "webfetch" tool.
type WebFetch struct {
	client         *http.Client
	defaultTimeout time.Duration
}

// New returns a WebFetch tool with a client built via newClient, which
// installs the dial-time SSRF guard on every connection attempt.
func New() *WebFetch {
	return &WebFetch{
		client:         newClient(),
		defaultTimeout: defaultTimeout,
	}
}

func (w *WebFetch) Name() string { return "webfetch" }

func (w *WebFetch) Description() string {
	return "Fetch a single URL via HTTP GET. HTML responses have their readable text extracted " +
		"(scripts, styles, and tags removed); other content types are returned raw. The response " +
		"body is capped at 100 KiB. Defaults to a 30s timeout. Only http and https URLs are " +
		"supported; requests to link-local and cloud-metadata addresses are blocked."
}

func (w *WebFetch) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"url":             {Type: "string", Description: "the http or https URL to fetch"},
			"timeout_seconds": {Type: "integer", Description: "max seconds before the fetch is aborted (default: 30)"},
		},
		Required: []string{"url"},
	}
}

// webfetchInput is the schema of WebFetch's Execute input.
type webfetchInput struct {
	URL            string `json:"url"`
	TimeoutSeconds int    `json:"timeout_seconds"`
}

// Execute issues a single HTTP GET for in.URL and returns its content as
// Text. Domain-level failures (bad input, unsupported scheme, non-2xx
// status, timeout, SSRF block, transport error) are all reported as
// Result{IsError: true} with a nil error; a non-nil error is returned only
// when ctx itself — the turn context, not the derived per-request deadline —
// has been cancelled, so the caller can distinguish "the fetch failed" from
// "the turn was aborted".
func (w *WebFetch) Execute(ctx context.Context, input json.RawMessage) (tool.Result, error) {
	var in webfetchInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("webfetch: invalid input: %v", err)}, nil
	}
	if strings.TrimSpace(in.URL) == "" {
		return tool.Result{IsError: true, Text: "webfetch: invalid input: url is required"}, nil
	}
	if in.TimeoutSeconds < 0 {
		return tool.Result{IsError: true, Text: "webfetch: invalid input: timeout_seconds must not be negative"}, nil
	}

	u, err := url.Parse(in.URL)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return tool.Result{IsError: true, Text: "webfetch: only http and https URLs are supported"}, nil
	}

	d := w.defaultTimeout
	if in.TimeoutSeconds > 0 {
		d = time.Duration(in.TimeoutSeconds) * time.Second
	}

	reqCtx, cancel := context.WithTimeout(ctx, d)
	defer cancel()

	req, err := http.NewRequestWithContext(reqCtx, http.MethodGet, u.String(), nil)
	if err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("webfetch: invalid input: %v", err)}, nil
	}
	req.Header.Set("User-Agent", userAgent)

	resp, err := w.client.Do(req)
	if err != nil {
		return mapFetchError(ctx, err, d)
	}
	defer func() { _ = resp.Body.Close() }()

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		snippet, _, _ := readCapped(resp.Body, maxErrorBodyBytes)
		text := "webfetch: request failed: " + resp.Status
		if s := strings.TrimSpace(string(snippet)); s != "" {
			text += "\n" + s
		}
		return tool.Result{IsError: true, Text: text}, nil
	}

	body, truncated, err := readCapped(resp.Body, maxBodyBytes)
	if err != nil {
		return mapFetchError(ctx, err, d)
	}

	text := string(body)
	if isHTML(resp.Header.Get("Content-Type")) {
		text = extractText(body)
	}
	if truncated {
		text += "\n[response truncated after 100 KiB]"
	}
	if strings.TrimSpace(text) == "" {
		text = fmt.Sprintf("(no content; %s)", resp.Status)
	}

	return tool.Result{Text: text}, nil
}

// readCapped reads r up to limit bytes. A body that reaches exactly limit
// bytes without hitting EOF is trimmed to limit and reported truncated; a
// body under the limit is returned intact. Reading limit+1 bytes is what
// distinguishes those two cases with a single ReadAll call.
func readCapped(r io.Reader, limit int64) (data []byte, truncated bool, err error) {
	data, err = io.ReadAll(io.LimitReader(r, limit+1))
	if err != nil {
		return nil, false, err
	}
	if int64(len(data)) > limit {
		return data[:limit], true, nil
	}
	return data, false, nil
}

// mapFetchError maps an error returned by w.client.Do (or by the subsequent
// body read) onto a tool.Result. The turn ctx is checked first — a real Go
// error there means the turn itself was cancelled and must abort, taking
// priority over the derived per-request deadline that also fired as a side
// effect of that cancellation. errHostNotAllowed's message deliberately
// carries no resolved IP; see ssrfControl for why.
func mapFetchError(ctx context.Context, err error, d time.Duration) (tool.Result, error) {
	if ctx.Err() != nil {
		return tool.Result{}, ctx.Err()
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return tool.Result{IsError: true, Text: fmt.Sprintf("webfetch: request timed out after %s", d)}, nil
	}
	if errors.Is(err, errHostNotAllowed) {
		return tool.Result{IsError: true, Text: "webfetch: host not allowed"}, nil
	}
	return tool.Result{IsError: true, Text: fmt.Sprintf("webfetch: request failed: %v", err)}, nil
}
