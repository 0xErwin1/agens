package openai

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strings"
	"sync"

	"github.com/iperez/agens/internal/provider"
)

// sseFramer decodes an OpenAI chat-completions Server-Sent Events body one
// "data:" line at a time.
//
// It uses bufio.Reader.ReadString rather than bufio.Scanner: Scanner's
// default token size truncates around 64KiB, which a long tool-call
// arguments payload can exceed. OpenAI never emits a data: event split
// across multiple lines, so one ReadString call per line is sufficient.
type sseFramer struct {
	r *bufio.Reader
}

// next returns the next SSE data payload with its "data: " prefix stripped.
// done reports that a "data: [DONE]" marker was read; payload is empty in
// that case. A nil payload/done pair with a non-nil err means the read
// failed: an io.EOF encountered before [DONE] is wrapped with
// io.ErrUnexpectedEOF, and any other read error is returned unchanged.
func (f *sseFramer) next() (payload string, done bool, err error) {
	for {
		line, err := f.r.ReadString('\n')
		if err != nil {
			if errors.Is(err, io.EOF) {
				return "", false, fmt.Errorf("openai: stream closed before [DONE]: %w", io.ErrUnexpectedEOF)
			}
			return "", false, err
		}

		line = strings.TrimRight(line, "\r\n")
		if payload, done, ok := decodeSSELine(line); ok {
			return payload, done, nil
		}
	}
}

// chatCompletionsStream implements provider.StreamReader over a
// chat-completions SSE response body. It decodes wire chunks into a FIFO
// queue of provider.StreamEvent, tracking the index-to-ToolCallID mapping
// required to resolve tool-call argument fragments that carry only their
// index.
type chatCompletionsStream struct {
	body   io.ReadCloser
	framer *sseFramer

	queue     []provider.StreamEvent
	toolIDs   map[int]string
	openOrder []int
	finished  bool

	err error

	closeOnce sync.Once
	closeErr  error
}

var _ provider.StreamReader = (*chatCompletionsStream)(nil)

// newStream builds a chatCompletionsStream that decodes body as an OpenAI
// chat-completions SSE response. body is closed by the returned stream's
// Close method.
func newStream(body io.ReadCloser) *chatCompletionsStream {
	return &chatCompletionsStream{
		body:    body,
		framer:  &sseFramer{r: bufio.NewReader(body)},
		toolIDs: make(map[int]string),
	}
}

// Recv implements provider.StreamReader.
func (s *chatCompletionsStream) Recv() (provider.StreamEvent, error) {
	if s.err != nil {
		return provider.StreamEvent{}, s.err
	}

	for len(s.queue) == 0 {
		payload, done, err := s.framer.next()
		if err != nil {
			s.err = err
			return provider.StreamEvent{}, s.err
		}
		if done {
			if s.finished {
				s.err = io.EOF
			} else {
				s.err = errors.New("openai: stream ended without finish_reason")
			}
			return provider.StreamEvent{}, s.err
		}

		var chunk wireChunk
		if err := json.Unmarshal([]byte(payload), &chunk); err != nil {
			s.err = fmt.Errorf("openai: malformed stream chunk: %w", err)
			return provider.StreamEvent{}, s.err
		}
		s.decodeChunk(chunk)
	}

	ev := s.queue[0]
	s.queue = s.queue[1:]
	return ev, nil
}

// decodeChunk translates one wire chunk into 0..N StreamEvent values,
// appended to s.queue in wire order.
func (s *chatCompletionsStream) decodeChunk(chunk wireChunk) {
	for _, choice := range chunk.Choices {
		if choice.Delta.Content != "" {
			s.queue = append(s.queue, provider.StreamEvent{
				Type: provider.EventTextDelta,
				Text: choice.Delta.Content,
			})
		}

		for _, tc := range choice.Delta.ToolCalls {
			id, known := s.toolIDs[tc.Index]
			if !known {
				id = tc.ID
				s.toolIDs[tc.Index] = id
				s.openOrder = append(s.openOrder, tc.Index)
				s.queue = append(s.queue, provider.StreamEvent{
					Type:       provider.EventToolCallStart,
					ToolCallID: id,
					ToolName:   tc.Function.Name,
				})
			}
			if tc.Function.Arguments != "" {
				s.queue = append(s.queue, provider.StreamEvent{
					Type:       provider.EventToolArgsDelta,
					ToolCallID: id,
					ArgsDelta:  tc.Function.Arguments,
				})
			}
		}

		if choice.FinishReason != nil {
			for _, idx := range s.openOrder {
				s.queue = append(s.queue, provider.StreamEvent{
					Type:       provider.EventToolCallEnd,
					ToolCallID: s.toolIDs[idx],
				})
			}
			s.openOrder = nil
			s.queue = append(s.queue, provider.StreamEvent{
				Type:       provider.EventDone,
				StopReason: *choice.FinishReason,
			})
			s.finished = true
		}
	}

	if chunk.Usage != nil {
		s.queue = append(s.queue, provider.StreamEvent{
			Type: provider.EventUsage,
			Usage: &provider.Usage{
				InputTokens:  chunk.Usage.PromptTokens,
				OutputTokens: chunk.Usage.CompletionTokens,
			},
		})
	}
}

// Close implements provider.StreamReader. It is safe to call more than
// once: only the first call closes the underlying body, and every call
// returns that first call's result.
func (s *chatCompletionsStream) Close() error {
	s.closeOnce.Do(func() {
		s.closeErr = s.body.Close()
	})
	return s.closeErr
}

// decodeSSELine classifies one already-trimmed SSE line. ok is false for
// lines that carry no payload (blank lines, comments, or any field other
// than "data:") and must be skipped by the caller.
func decodeSSELine(line string) (payload string, done bool, ok bool) {
	if line == "" || strings.HasPrefix(line, ":") {
		return "", false, false
	}
	data, isData := strings.CutPrefix(line, "data: ")
	if !isData {
		return "", false, false
	}
	if data == "[DONE]" {
		return "", true, true
	}
	return data, false, true
}
