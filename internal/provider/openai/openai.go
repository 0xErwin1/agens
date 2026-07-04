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

	// DefaultModel is the model id used when a caller (for example the
	// agent loop's model precedence) needs a provider default without
	// constructing a Provider first.
	DefaultModel = "gpt-4.1"

	// maxErrorBodyBytes bounds how much of a non-2xx response body Stream
	// reads before handing it to parseResponseError.
	maxErrorBodyBytes = 32 * 1024
)

// chatModelIDPrefixes are the "/models" id prefixes Models keeps. The
// endpoint returns every model OpenAI hosts — embeddings, whisper, tts,
// image, and moderation models included — with no way to filter server-side,
// so the chat-capable subset is selected by id prefix instead.
var chatModelIDPrefixes = []string{"gpt-", "chatgpt", "o1", "o3", "o4"}

// isChatModel reports whether id names a chat-capable model, based on
// chatModelIDPrefixes.
func isChatModel(id string) bool {
	for _, prefix := range chatModelIDPrefixes {
		if strings.HasPrefix(id, prefix) {
			return true
		}
	}
	return false
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

// Models implements provider.Provider, fetching the current model catalog
// from OpenAI's "/models" endpoint at call time rather than returning a
// hardcoded list. The endpoint reports only an id per model — no context
// window or pricing — so ContextWindow and MaxOutputTokens are left at zero,
// and results are filtered to the chat-capable subset (see
// chatModelIDPrefixes).
func (p *Provider) Models(ctx context.Context) ([]provider.ModelInfo, error) {
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodGet, p.baseURL+"/models", nil)
	if err != nil {
		return nil, fmt.Errorf("openai: build models request: %w", err)
	}
	httpReq.Header.Set("Accept", "application/json")

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
		return nil, fmt.Errorf("openai: fetch models: %w", err)
	}
	defer func() { _ = resp.Body.Close() }()

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		errBody, readErr := io.ReadAll(io.LimitReader(resp.Body, maxErrorBodyBytes))
		if readErr != nil {
			return nil, fmt.Errorf("openai: read error response body: %w", readErr)
		}
		return nil, parseResponseError(resp.StatusCode, errBody)
	}

	var wire wireModelsResponse
	if err := json.NewDecoder(resp.Body).Decode(&wire); err != nil {
		return nil, fmt.Errorf("openai: decode models response: %w", err)
	}

	models := make([]provider.ModelInfo, 0, len(wire.Data))
	for _, m := range wire.Data {
		if !isChatModel(m.ID) {
			continue
		}
		models = append(models, provider.ModelInfo{
			ID:            m.ID,
			DisplayName:   m.ID,
			SupportsTools: true,
		})
	}
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
