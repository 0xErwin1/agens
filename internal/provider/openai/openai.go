package openai

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/iperez/agens/internal/provider"
)

const (
	defaultBaseURL = "https://api.openai.com/v1"

	// maxErrorBodyBytes bounds how much of a non-2xx response body Stream
	// reads before handing it to parseResponseError.
	maxErrorBodyBytes = 32 * 1024
)

// staticModels is the provisional model catalog. AGN-7 replaces it with a
// models.dev-backed lookup; until then, context/output limits and
// SupportsTools are hand-maintained best-effort values.
var staticModels = []provider.ModelInfo{
	{ID: "gpt-4.1", DisplayName: "GPT-4.1", ContextWindow: 1_000_000, MaxOutputTokens: 32_768, SupportsTools: true},
	{ID: "gpt-4.1-mini", DisplayName: "GPT-4.1 mini", ContextWindow: 1_000_000, MaxOutputTokens: 32_768, SupportsTools: true},
	{ID: "gpt-4o", DisplayName: "GPT-4o", ContextWindow: 128_000, MaxOutputTokens: 16_384, SupportsTools: true},
	{ID: "o4-mini", DisplayName: "o4-mini", ContextWindow: 200_000, MaxOutputTokens: 100_000, SupportsTools: true},
}

// Provider implements provider.Provider against OpenAI's chat-completions
// API, authenticated with the given provider.Authenticator.
type Provider struct {
	baseURL    string
	model      string
	httpClient *http.Client
	auth       provider.Authenticator
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
		return nil, errors.New("openai: authenticator must not be nil")
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
	}, nil
}

// ID implements provider.Provider.
func (p *Provider) ID() string {
	return "openai-api"
}

// Models implements provider.Provider, returning a copy of the static
// catalog so callers cannot mutate Provider's internal state.
func (p *Provider) Models(_ context.Context) ([]provider.ModelInfo, error) {
	models := make([]provider.ModelInfo, len(staticModels))
	copy(models, staticModels)
	return models, nil
}

// Stream implements provider.Provider.
func (p *Provider) Stream(ctx context.Context, req provider.ChatRequest) (provider.StreamReader, error) {
	model := req.Model
	if model == "" {
		model = p.model
	}
	if model == "" {
		return nil, errors.New("openai: no model resolved: request.Model and provider default are both empty")
	}
	req.Model = model

	wire, err := encodeRequest(req)
	if err != nil {
		return nil, fmt.Errorf("openai: encode request: %w", err)
	}

	body, err := json.Marshal(wire)
	if err != nil {
		return nil, fmt.Errorf("openai: marshal request: %w", err)
	}

	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, p.baseURL+"/chat/completions", bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("openai: build request: %w", err)
	}
	httpReq.Header.Set("Content-Type", "application/json")
	httpReq.Header.Set("Accept", "text/event-stream")

	if !p.auth.Valid(time.Now()) {
		if err := p.auth.Refresh(ctx); err != nil {
			return nil, fmt.Errorf("openai: refresh credential: %w", err)
		}
	}
	if err := p.auth.Decorate(ctx, httpReq); err != nil {
		return nil, fmt.Errorf("openai: decorate request: %w", err)
	}

	resp, err := p.httpClient.Do(httpReq)
	if err != nil {
		return nil, fmt.Errorf("openai: send request: %w", err)
	}

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		errBody, readErr := io.ReadAll(io.LimitReader(resp.Body, maxErrorBodyBytes))
		_ = resp.Body.Close()
		if readErr != nil {
			return nil, fmt.Errorf("openai: read error response body: %w", readErr)
		}
		return nil, parseResponseError(resp.StatusCode, errBody)
	}

	return newStream(resp.Body), nil
}
