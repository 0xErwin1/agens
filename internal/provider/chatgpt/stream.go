package chatgpt

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strings"
	"sync"

	"github.com/0xErwin1/agens/internal/provider"
)

// IncompleteResponseError reports that a /responses stream ended via
// "response.incomplete" instead of "response.completed", for example because
// a max-output-tokens limit cut the response short.
type IncompleteResponseError struct {
	Reason string
}

func (e *IncompleteResponseError) Error() string {
	return fmt.Sprintf("chatgpt: incomplete response: %s", e.Reason)
}

// sseFramer decodes a /responses Server-Sent Events body one "data:" line at
// a time.
//
// Unlike the chat-completions wire, /responses has no "[DONE]" sentinel:
// termination is signaled entirely by the JSON payload's own "type" field
// (response.completed, response.failed, response.incomplete). next therefore
// only ever returns a payload or a terminal error, never a "done" flag.
type sseFramer struct {
	r *bufio.Reader
}

// next returns the next SSE data payload with its "data: " prefix stripped.
// An io.EOF encountered mid-stream is wrapped with io.ErrUnexpectedEOF: the
// caller is expected to have already reached a terminal event by the time
// the underlying body closes cleanly.
func (f *sseFramer) next() (payload string, err error) {
	for {
		line, err := f.r.ReadString('\n')
		if err != nil {
			if errors.Is(err, io.EOF) {
				return "", fmt.Errorf("chatgpt: stream closed before response.completed: %w", io.ErrUnexpectedEOF)
			}
			return "", err
		}

		line = strings.TrimRight(line, "\r\n")
		if payload, ok := decodeSSELine(line); ok {
			return payload, nil
		}
	}
}

// decodeSSELine classifies one already-trimmed SSE line. ok is false for
// lines that carry no payload (blank lines, comments, or any field other
// than "data:") and must be skipped by the caller.
func decodeSSELine(line string) (payload string, ok bool) {
	if line == "" || strings.HasPrefix(line, ":") {
		return "", false
	}
	data, isData := strings.CutPrefix(line, "data: ")
	if !isData {
		return "", false
	}
	return data, true
}

// responsesStream implements provider.StreamReader over a /responses SSE
// response body. It decodes wire events into a FIFO queue of
// provider.StreamEvent.
type responsesStream struct {
	body   io.ReadCloser
	framer *sseFramer

	queue       []provider.StreamEvent
	sawToolCall bool
	finished    bool

	err error

	closeOnce sync.Once
	closeErr  error
}

var _ provider.StreamReader = (*responsesStream)(nil)

// newResponsesStream builds a responsesStream that decodes body as a
// /responses SSE response. body is closed by the returned stream's Close
// method.
func newResponsesStream(body io.ReadCloser) *responsesStream {
	return &responsesStream{
		body:   body,
		framer: &sseFramer{r: bufio.NewReader(body)},
	}
}

// Recv implements provider.StreamReader.
func (s *responsesStream) Recv() (provider.StreamEvent, error) {
	if s.err != nil {
		return provider.StreamEvent{}, s.err
	}

	for len(s.queue) == 0 {
		if s.finished {
			s.err = io.EOF
			return provider.StreamEvent{}, s.err
		}

		payload, err := s.framer.next()
		if err != nil {
			s.err = err
			return provider.StreamEvent{}, s.err
		}

		var event wireStreamEvent
		if err := json.Unmarshal([]byte(payload), &event); err != nil {
			s.err = fmt.Errorf("chatgpt: malformed stream event: %w", err)
			return provider.StreamEvent{}, s.err
		}

		if err := s.decodeEvent(event); err != nil {
			s.err = err
			return provider.StreamEvent{}, s.err
		}
	}

	ev := s.queue[0]
	s.queue = s.queue[1:]
	return ev, nil
}

// decodeEvent translates one wire event into 0..N StreamEvent values
// appended to s.queue, or returns a terminal error for response.failed and
// response.incomplete.
func (s *responsesStream) decodeEvent(event wireStreamEvent) error {
	switch event.Type {
	case "response.output_text.delta":
		s.queue = append(s.queue, provider.StreamEvent{
			Type: provider.EventTextDelta,
			Text: event.Delta,
		})

	case "response.reasoning_summary_text.delta", "response.reasoning_text.delta":
		s.queue = append(s.queue, provider.StreamEvent{
			Type: provider.EventReasoningDelta,
			Text: event.Delta,
		})

	case "response.output_item.added":
		if event.Item != nil && event.Item.Type == responseItemTypeFunctionCall {
			s.sawToolCall = true
			s.queue = append(s.queue, provider.StreamEvent{
				Type:       provider.EventToolCallStart,
				ToolCallID: event.Item.CallID,
				ToolName:   event.Item.Name,
			})
		}

	case "response.output_item.done":
		if event.Item != nil && event.Item.Type == responseItemTypeFunctionCall {
			// Unlike chat-completions' incrementally streamed argument
			// fragments, the Responses API delivers a function call's
			// arguments whole in this single event.
			s.queue = append(s.queue,
				provider.StreamEvent{
					Type:       provider.EventToolArgsDelta,
					ToolCallID: event.Item.CallID,
					ArgsDelta:  event.Item.Arguments,
				},
				provider.StreamEvent{
					Type:       provider.EventToolCallEnd,
					ToolCallID: event.Item.CallID,
				},
			)
		}

	case "response.completed":
		if event.Response != nil && event.Response.Usage != nil {
			s.queue = append(s.queue, provider.StreamEvent{
				Type: provider.EventUsage,
				Usage: &provider.Usage{
					InputTokens:  event.Response.Usage.InputTokens,
					OutputTokens: event.Response.Usage.OutputTokens,
				},
			})
		}

		stopReason := "stop"
		if s.sawToolCall {
			stopReason = "tool_calls"
		}
		s.queue = append(s.queue, provider.StreamEvent{
			Type:       provider.EventDone,
			StopReason: stopReason,
		})
		s.finished = true

	case "response.failed":
		var code, message string
		if event.Response != nil && event.Response.Error != nil {
			code = event.Response.Error.Code
			message = event.Response.Error.Message
		}
		return &ResponseError{Code: code, Message: message}

	case "response.incomplete":
		var reason string
		if event.Response != nil && event.Response.IncompleteDetails != nil {
			reason = event.Response.IncompleteDetails.Reason
		}
		return &IncompleteResponseError{Reason: reason}
	}

	return nil
}

// Close implements provider.StreamReader. It is safe to call more than
// once: only the first call closes the underlying body, and every call
// returns that first call's result.
func (s *responsesStream) Close() error {
	s.closeOnce.Do(func() {
		s.closeErr = s.body.Close()
	})
	return s.closeErr
}
