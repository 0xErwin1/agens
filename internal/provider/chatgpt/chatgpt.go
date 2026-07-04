package chatgpt

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"runtime"
	"strings"
	"time"

	"github.com/google/uuid"

	"github.com/iperez/agens/internal/provider"
)

const (
	defaultBaseURL = "https://chatgpt.com/backend-api/codex"

	// DefaultModel is the model id used when neither the request nor the
	// provider's Config specifies one. The ChatGPT-subscription backend only
	// serves a rotating subset of ids (e.g. gpt-5.5, gpt-5.4) and rejects
	// others with a 400; Config.Model or the --model flag overrides this
	// whenever the current default is no longer served. A runtime models
	// lookup (AGN-7) will eventually replace this hardcoded default.
	DefaultModel = "gpt-5.5"

	// codexCLIVersion is the version segment reported in the User-Agent
	// header and sent as the /models client_version query parameter. It has
	// no functional meaning within agens and is not tied to any agens
	// release. The backend's /models endpoint gates its response on this
	// value: a stale version (e.g. the previous "0.1.0") makes /models
	// return an empty list, so this must be bumped periodically to match a
	// recent codex CLI release.
	codexCLIVersion = "0.142.2"

	// maxErrorBodyBytes bounds how much of a non-2xx response body Stream
	// reads before handing it to parseResponseError.
	maxErrorBodyBytes = 32 * 1024
)

// Provider implements provider.Provider against OpenAI's Responses API
// ("/responses"), authenticated with the given provider.Authenticator.
type Provider struct {
	baseURL    string
	model      string
	httpClient *http.Client
	auth       provider.Authenticator
	sessionID  string
}

var _ provider.Provider = (*Provider)(nil)

// New builds a Provider from cfg and auth. It matches the
// provider.ProviderFactory signature. It returns an error if auth is nil.
//
// cfg.HTTPTimeout is applied verbatim to the underlying http.Client: it
// bounds the entire Stream call, including the full duration of the SSE
// response, not just the initial connection — fine-grained cancellation is
// the caller's responsibility via ctx.
func New(cfg provider.Config, auth provider.Authenticator) (provider.Provider, error) {
	if auth == nil {
		return nil, errors.New("chatgpt: authenticator must not be nil")
	}

	baseURL := cfg.BaseURL
	if baseURL == "" {
		baseURL = defaultBaseURL
	}
	baseURL = strings.TrimSuffix(baseURL, "/")

	return &Provider{
		baseURL:    baseURL,
		model:      cfg.Model,
		httpClient: &http.Client{Timeout: cfg.HTTPTimeout},
		auth:       auth,
		sessionID:  uuid.NewString(),
	}, nil
}

// ID implements provider.Provider.
func (p *Provider) ID() string {
	return "openai-chatgpt"
}

// EffortLevels implements provider.Provider: the reasoning-effort values the
// OpenAI Responses backend accepts, in ascending order.
func (p *Provider) EffortLevels() []string {
	return []string{"minimal", "low", "medium", "high", "xhigh"}
}

// Models implements provider.Provider, fetching the current model catalog
// from the backend's "/models" endpoint at call time rather than returning a
// hardcoded list. Only entries with visibility "list" are returned; hidden
// or other visibilities are filtered out.
func (p *Provider) Models(ctx context.Context) ([]provider.ModelInfo, error) {
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodGet, p.baseURL+"/models", nil)
	if err != nil {
		return nil, fmt.Errorf("chatgpt: build models request: %w", err)
	}

	query := httpReq.URL.Query()
	query.Set("client_version", codexCLIVersion)
	httpReq.URL.RawQuery = query.Encode()

	httpReq.Header.Set("Accept", "application/json")
	httpReq.Header.Set("originator", "codex_cli_rs")
	httpReq.Header.Set("User-Agent", userAgent())

	if !p.auth.Valid(time.Now()) {
		if err := p.auth.Refresh(ctx); err != nil {
			return nil, fmt.Errorf("chatgpt: refresh credential: %w", err)
		}
	}
	if err := p.auth.Decorate(ctx, httpReq); err != nil {
		return nil, fmt.Errorf("chatgpt: decorate request: %w", err)
	}

	resp, err := p.httpClient.Do(httpReq)
	if err != nil {
		return nil, fmt.Errorf("chatgpt: fetch models: %w", err)
	}
	defer func() { _ = resp.Body.Close() }()

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		errBody, readErr := io.ReadAll(io.LimitReader(resp.Body, maxErrorBodyBytes))
		if readErr != nil {
			return nil, fmt.Errorf("chatgpt: read error response body: %w", readErr)
		}
		return nil, parseResponseError(resp.StatusCode, errBody)
	}

	var wire wireModelsResponse
	if err := json.NewDecoder(resp.Body).Decode(&wire); err != nil {
		return nil, fmt.Errorf("chatgpt: decode models response: %w", err)
	}

	models := make([]provider.ModelInfo, 0, len(wire.Models))
	for _, m := range wire.Models {
		if m.Visibility != "list" {
			continue
		}
		models = append(models, provider.ModelInfo{
			ID:            m.Slug,
			DisplayName:   m.DisplayName,
			ContextWindow: m.ContextWindow,
			SupportsTools: true,
		})
	}
	return models, nil
}

// userAgent builds the User-Agent header value sent with every /responses
// request. This exact string, along with the session-id header set in
// Stream, is a best-effort match of the codex CLI's own client fingerprint
// and may need adjustment if the backend starts treating it differently.
func userAgent() string {
	return fmt.Sprintf("codex_cli_rs/%s (%s %s)", codexCLIVersion, runtime.GOOS, runtime.GOARCH)
}

// Stream implements provider.Provider.
func (p *Provider) Stream(ctx context.Context, req provider.ChatRequest) (provider.StreamReader, error) {
	model := req.Model
	if model == "" {
		model = p.model
	}
	if model == "" {
		model = DefaultModel
	}
	req.Model = model

	wire, err := encodeRequest(req)
	if err != nil {
		return nil, fmt.Errorf("chatgpt: encode request: %w", err)
	}

	body, err := json.Marshal(wire)
	if err != nil {
		return nil, fmt.Errorf("chatgpt: marshal request: %w", err)
	}

	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, p.baseURL+"/responses", bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("chatgpt: build request: %w", err)
	}
	httpReq.Header.Set("Content-Type", "application/json")
	httpReq.Header.Set("Accept", "text/event-stream")
	httpReq.Header.Set("originator", "codex_cli_rs")
	httpReq.Header.Set("User-Agent", userAgent())
	httpReq.Header.Set("session-id", p.sessionID)

	if !p.auth.Valid(time.Now()) {
		if err := p.auth.Refresh(ctx); err != nil {
			return nil, fmt.Errorf("chatgpt: refresh credential: %w", err)
		}
	}
	if err := p.auth.Decorate(ctx, httpReq); err != nil {
		return nil, fmt.Errorf("chatgpt: decorate request: %w", err)
	}

	resp, err := p.httpClient.Do(httpReq)
	if err != nil {
		return nil, fmt.Errorf("chatgpt: send request: %w", err)
	}

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		errBody, readErr := io.ReadAll(io.LimitReader(resp.Body, maxErrorBodyBytes))
		_ = resp.Body.Close()
		if readErr != nil {
			return nil, fmt.Errorf("chatgpt: read error response body: %w", readErr)
		}
		return nil, parseResponseError(resp.StatusCode, errBody)
	}

	return newResponsesStream(resp.Body), nil
}
