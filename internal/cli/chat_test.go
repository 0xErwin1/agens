package cli

import (
	"bytes"
	"context"
	"errors"
	"io"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/agent"
	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// chatStreamStep is one scripted Recv() outcome, mirroring
// internal/agentloop's fakes_test.go streamStep.
type chatStreamStep struct {
	ev  provider.StreamEvent
	err error
}

// chatScriptedStream is a provider.StreamReader over a fixed slice of
// steps, mirroring internal/agentloop's scriptedStream.
type chatScriptedStream struct {
	steps []chatStreamStep
	idx   int
}

var _ provider.StreamReader = (*chatScriptedStream)(nil)

func newChatScriptedStream(steps []chatStreamStep) *chatScriptedStream {
	return &chatScriptedStream{steps: steps}
}

func (s *chatScriptedStream) Recv() (provider.StreamEvent, error) {
	if s.idx >= len(s.steps) {
		return provider.StreamEvent{}, io.EOF
	}
	step := s.steps[s.idx]
	s.idx++
	if step.err != nil {
		return provider.StreamEvent{}, step.err
	}
	return step.ev, nil
}

func (s *chatScriptedStream) Close() error { return nil }

// chatFakeProvider is a provider.Provider whose Stream returns a
// chatScriptedStream, recording the ChatRequest it received.
type chatFakeProvider struct {
	steps []chatStreamStep

	lastRequest provider.ChatRequest
}

var _ provider.Provider = (*chatFakeProvider)(nil)

func (p *chatFakeProvider) ID() string { return "chat-fake-provider" }

func (p *chatFakeProvider) Models(context.Context) ([]provider.ModelInfo, error) { return nil, nil }

func (p *chatFakeProvider) Stream(_ context.Context, req provider.ChatRequest) (provider.StreamReader, error) {
	p.lastRequest = req
	return newChatScriptedStream(p.steps), nil
}

func textDeltaSteps(chunks ...string) []chatStreamStep {
	steps := make([]chatStreamStep, 0, len(chunks)+1)
	for _, c := range chunks {
		steps = append(steps, chatStreamStep{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: c}})
	}
	steps = append(steps, chatStreamStep{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "stop"}})
	return steps
}

func TestChatCommand_RejectsMoreThanOneArg(t *testing.T) {
	cmd := newChatCommandWithBuilder(func(agent.Options) (*agentloop.Loop, error) {
		t.Fatal("builder should not be called when arg validation fails")
		return nil, nil
	})
	buf := new(bytes.Buffer)
	cmd.SetOut(buf)
	cmd.SetErr(buf)
	cmd.SetArgs([]string{"one", "two"})

	if err := cmd.Execute(); err == nil {
		t.Fatal("Execute() error = nil, want an error for more than one positional arg")
	}
}

func TestChatCommand_ArgPromptDrivesHistoryAndOutput(t *testing.T) {
	fakeProvider := &chatFakeProvider{steps: textDeltaSteps("hello")}
	build := func(agent.Options) (*agentloop.Loop, error) {
		return agentloop.New(fakeProvider, nil, agentloop.WithModel("gpt-test")), nil
	}

	cmd := newChatCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"hi there"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if got, want := out.String(), "hello\n"; got != want {
		t.Fatalf("stdout = %q, want %q", got, want)
	}

	if len(fakeProvider.lastRequest.Messages) != 1 {
		t.Fatalf("provider received %d messages, want 1", len(fakeProvider.lastRequest.Messages))
	}
	got := fakeProvider.lastRequest.Messages[0]
	if got.Role != message.RoleUser {
		t.Fatalf("history[0].Role = %q, want %q", got.Role, message.RoleUser)
	}
	if len(got.Parts) != 1 {
		t.Fatalf("history[0].Parts = %+v, want exactly one TextPart", got.Parts)
	}
	text, ok := got.Parts[0].(message.TextPart)
	if !ok {
		t.Fatalf("history[0].Parts[0] = %T, want message.TextPart", got.Parts[0])
	}
	if text.Text != "hi there" {
		t.Fatalf("history[0] text = %q, want %q", text.Text, "hi there")
	}
}

func TestChatCommand_StdinPromptWhenNoArg(t *testing.T) {
	fakeProvider := &chatFakeProvider{steps: textDeltaSteps("ok")}
	build := func(agent.Options) (*agentloop.Loop, error) {
		return agentloop.New(fakeProvider, nil, agentloop.WithModel("gpt-test")), nil
	}

	cmd := newChatCommandWithBuilder(build)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetIn(strings.NewReader("  from stdin  \n"))
	cmd.SetArgs(nil)

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if len(fakeProvider.lastRequest.Messages) != 1 {
		t.Fatalf("provider received %d messages, want 1", len(fakeProvider.lastRequest.Messages))
	}
	text := fakeProvider.lastRequest.Messages[0].Parts[0].(message.TextPart)
	if text.Text != "from stdin" {
		t.Fatalf("prompt from stdin = %q, want trimmed %q", text.Text, "from stdin")
	}
}

func TestChatCommand_NoArgNoStdinIsAnError(t *testing.T) {
	build := func(agent.Options) (*agentloop.Loop, error) {
		t.Fatal("builder should not be called when there is no prompt")
		return nil, nil
	}

	cmd := newChatCommandWithBuilder(build)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetIn(strings.NewReader(""))
	cmd.SetArgs(nil)

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want an error when neither an arg nor stdin supplies a prompt")
	}
	if !strings.Contains(err.Error(), "prompt is required") {
		t.Fatalf("Execute() error = %q, want it to mention a required prompt", err.Error())
	}
}

func TestChatCommand_SinkPrintsMultipleDeltasThenNewline(t *testing.T) {
	fakeProvider := &chatFakeProvider{steps: textDeltaSteps("he", "llo", " world")}
	build := func(agent.Options) (*agentloop.Loop, error) {
		return agentloop.New(fakeProvider, nil, agentloop.WithModel("gpt-test")), nil
	}

	cmd := newChatCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"prompt"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if got, want := out.String(), "hello world\n"; got != want {
		t.Fatalf("stdout = %q, want %q", got, want)
	}
}

func TestChatCommand_FlagsReachBuilderOptions(t *testing.T) {
	fakeProvider := &chatFakeProvider{steps: textDeltaSteps("ok")}
	var received agent.Options
	build := func(opts agent.Options) (*agentloop.Loop, error) {
		received = opts
		return agentloop.New(fakeProvider, nil, agentloop.WithModel("gpt-test")), nil
	}

	cmd := newChatCommandWithBuilder(build)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"--model", "gpt-custom", "--system", "custom prompt", "hi"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if received.Model != "gpt-custom" {
		t.Fatalf("Options.Model = %q, want %q", received.Model, "gpt-custom")
	}
	if received.SystemPrompt != "custom prompt" {
		t.Fatalf("Options.SystemPrompt = %q, want %q", received.SystemPrompt, "custom prompt")
	}
}

func TestChatCommand_BuilderErrorPropagatesWithoutLeakingKey(t *testing.T) {
	const secret = "sk-should-never-appear"
	build := func(agent.Options) (*agentloop.Loop, error) {
		return nil, errors.New("agent: no credentials for provider \"openai-api\"")
	}

	cmd := newChatCommandWithBuilder(build)
	out := new(bytes.Buffer)
	errOut := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(errOut)
	cmd.SetArgs([]string{"hi"})

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the builder error to propagate")
	}
	if strings.Contains(out.String()+errOut.String(), secret) {
		t.Fatal("command output must never contain a raw api key value")
	}
}

func TestChatCommand_CanceledContextStopsBeforeStreaming(t *testing.T) {
	fakeProvider := &chatFakeProvider{steps: textDeltaSteps("should not be sent")}
	build := func(agent.Options) (*agentloop.Loop, error) {
		return agentloop.New(fakeProvider, nil, agentloop.WithModel("gpt-test")), nil
	}

	cmd := newChatCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"hi"})

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	cmd.SetContext(ctx)

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the canceled context to stop the run")
	}
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Execute() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if strings.Contains(out.String(), "should not be sent") {
		t.Fatalf("stdout = %q, must not contain streamed text since Run must stop before the fake provider is ever called", out.String())
	}
}
