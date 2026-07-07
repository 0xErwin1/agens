package bash

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/tool"
)

func TestNew_PanicsOnEmptyProjectRoot(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf(`New("") did not panic, want a panic`)
		}
	}()
	New("")
}

func TestBash_Name(t *testing.T) {
	b := New(t.TempDir())
	if got := b.Name(); got != "bash" {
		t.Fatalf("Name() = %q, want %q", got, "bash")
	}
}

func TestBash_Description(t *testing.T) {
	b := New(t.TempDir())
	desc := b.Description()
	lower := strings.ToLower(desc)

	for _, want := range []string{"bash -c", "project root", "combined", "120", "not sandboxed", "ask"} {
		if !strings.Contains(lower, want) {
			t.Fatalf("Description() = %q, want it to mention %q", desc, want)
		}
	}
}

func TestBash_Schema(t *testing.T) {
	b := New(t.TempDir())
	schema := b.Schema()
	if schema == nil {
		t.Fatalf("Schema() = nil, want non-nil")
	}
	if schema.Type != "object" {
		t.Fatalf("Schema().Type = %q, want %q", schema.Type, "object")
	}
	if _, ok := schema.Properties["command"]; !ok {
		t.Fatalf("Schema().Properties missing %q", "command")
	}
	if _, ok := schema.Properties["timeout_seconds"]; !ok {
		t.Fatalf("Schema().Properties missing %q", "timeout_seconds")
	}
	if len(schema.Required) != 1 || schema.Required[0] != "command" {
		t.Fatalf("Schema().Required = %v, want [%q]", schema.Required, "command")
	}
}

func TestBash_Execute_InputValidation(t *testing.T) {
	b := New(t.TempDir())

	tests := []struct {
		name  string
		input string
	}{
		{name: "invalid JSON", input: `{"command":`},
		{name: "missing command", input: `{}`},
		{name: "empty command", input: `{"command":""}`},
		{name: "whitespace-only command", input: `{"command":"   "}`},
		{name: "negative timeout", input: `{"command":"echo hi","timeout_seconds":-1}`},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			res, err := b.Execute(context.Background(), json.RawMessage(tt.input))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil (domain failures must be IsError, not a Go error)", err)
			}
			if !res.IsError {
				t.Fatalf("Execute(%s) IsError = false, want true (Text = %q)", tt.input, res.Text)
			}
		})
	}
}

func TestBash_Execute_HappyPath(t *testing.T) {
	b := New(t.TempDir())

	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"echo hello"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "hello") {
		t.Fatalf("Execute() Text = %q, want it to contain %q", res.Text, "hello")
	}
}

func TestBash_Execute_NonZeroExit(t *testing.T) {
	b := New(t.TempDir())

	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"echo before-exit; exit 3"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !res.IsError {
		t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "before-exit") {
		t.Fatalf("Execute() Text = %q, want it to still contain the produced output", res.Text)
	}
	if !strings.Contains(res.Text, "exit status 3") {
		t.Fatalf("Execute() Text = %q, want it to contain %q", res.Text, "exit status 3")
	}
}

func TestBash_Execute_CombinedStreams(t *testing.T) {
	b := New(t.TempDir())

	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"echo out; echo err >&2"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "out") {
		t.Fatalf("Execute() Text = %q, want it to contain stdout output %q", res.Text, "out")
	}
	if !strings.Contains(res.Text, "err") {
		t.Fatalf("Execute() Text = %q, want it to contain stderr output %q", res.Text, "err")
	}
}

func TestBash_Execute_Cwd(t *testing.T) {
	root := t.TempDir()
	want, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatalf("filepath.EvalSymlinks(%q) error = %v", root, err)
	}

	b := New(root)
	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"pwd"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, want) {
		t.Fatalf("Execute() Text = %q, want it to contain cwd %q", res.Text, want)
	}
}

// jsonCommand marshals a {"command": ...} input for Execute, failing the
// test on a marshal error.
func jsonCommand(t *testing.T, command string) json.RawMessage {
	t.Helper()
	data, err := json.Marshal(map[string]string{"command": command})
	if err != nil {
		t.Fatalf("marshal command input: %v", err)
	}
	return json.RawMessage(data)
}

// readPID reads and parses a single PID written by "echo $! > path".
func readPID(t *testing.T, path string) int {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read pid file %q: %v", path, err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
	if err != nil {
		t.Fatalf("parse pid %q: %v", data, err)
	}
	return pid
}

// killIfAlive force-kills pid, ignoring errors: used as test cleanup so a
// test that fails an assertion before the process naturally dies never
// leaks a background process into the rest of the suite.
func killIfAlive(pid int) {
	if pid > 0 {
		_ = syscall.Kill(pid, syscall.SIGKILL)
	}
}

// waitProcessDead polls syscall.Kill(pid, 0) until it reports ESRCH (no
// such process) or timeout elapses. A single sample is not reliable here:
// SIGKILL reaping is asynchronous and the process can remain visible as a
// zombie for a short window, so this uses deadline-based polling instead.
func waitProcessDead(t *testing.T, pid int, timeout time.Duration) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if err := syscall.Kill(pid, 0); err == syscall.ESRCH {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("process %d still alive after %s", pid, timeout)
}

func TestBash_Execute_Timeout(t *testing.T) {
	b := New(t.TempDir())
	b.defaultTimeout = 200 * time.Millisecond

	res, err := b.Execute(context.Background(), jsonCommand(t, "sleep 30"))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil (a timeout must not abort the turn)", err)
	}
	if !res.IsError {
		t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "timed out") {
		t.Fatalf("Execute() Text = %q, want it to mention a timeout", res.Text)
	}
}

func TestBash_Execute_Cancellation(t *testing.T) {
	b := New(t.TempDir())
	ctx, cancel := context.WithCancel(context.Background())

	type execResult struct {
		res tool.Result
		err error
	}
	done := make(chan execResult, 1)
	go func() {
		res, err := b.Execute(ctx, jsonCommand(t, "sleep 30"))
		done <- execResult{res, err}
	}()

	time.Sleep(100 * time.Millisecond)
	cancel()

	select {
	case r := <-done:
		if !errors.Is(r.err, context.Canceled) {
			t.Fatalf("Execute() error = %v, want errors.Is(err, context.Canceled)", r.err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Execute() did not return within 5s of cancellation")
	}
}

func TestBash_ProcessGroupKill_Timeout(t *testing.T) {
	dir := t.TempDir()
	b := New(dir)
	b.defaultTimeout = 200 * time.Millisecond

	pidFile := filepath.Join(dir, "pid.txt")
	res, err := b.Execute(context.Background(), jsonCommand(t, fmt.Sprintf("sleep 30 & echo $! > %s; wait", pidFile)))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !res.IsError {
		t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
	}

	pid := readPID(t, pidFile)
	t.Cleanup(func() { killIfAlive(pid) })
	waitProcessDead(t, pid, 2*time.Second)
}

func TestBash_ProcessGroupKill_Cancel(t *testing.T) {
	dir := t.TempDir()
	b := New(dir)
	ctx, cancel := context.WithCancel(context.Background())

	pidFile := filepath.Join(dir, "pid.txt")

	type execResult struct {
		res tool.Result
		err error
	}
	done := make(chan execResult, 1)
	go func() {
		res, err := b.Execute(ctx, jsonCommand(t, fmt.Sprintf("sleep 30 & echo $! > %s; wait", pidFile)))
		done <- execResult{res, err}
	}()

	var pid int
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if data, statErr := os.ReadFile(pidFile); statErr == nil && strings.TrimSpace(string(data)) != "" {
			pid = readPID(t, pidFile)
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if pid == 0 {
		t.Fatal("pid file was never populated before the polling deadline")
	}
	t.Cleanup(func() { killIfAlive(pid) })

	cancel()

	select {
	case r := <-done:
		if !errors.Is(r.err, context.Canceled) {
			t.Fatalf("Execute() error = %v, want errors.Is(err, context.Canceled)", r.err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Execute() did not return within 5s of cancellation")
	}

	waitProcessDead(t, pid, 2*time.Second)
}

func TestBash_Execute_WaitDelayIncompleteOutput(t *testing.T) {
	dir := t.TempDir()
	b := New(dir)
	b.waitDelay = 200 * time.Millisecond

	pidFile := filepath.Join(dir, "pid.txt")
	input := jsonCommand(t, fmt.Sprintf("echo hi; sleep 10 & echo $! > %s", pidFile))

	type execResult struct {
		res tool.Result
		err error
	}
	done := make(chan execResult, 1)
	go func() {
		res, err := b.Execute(context.Background(), input)
		done <- execResult{res, err}
	}()

	select {
	case r := <-done:
		if r.err != nil {
			t.Fatalf("Execute() error = %v, want nil", r.err)
		}
		if !strings.Contains(r.res.Text, "hi") {
			t.Fatalf("Execute() Text = %q, want it to contain %q", r.res.Text, "hi")
		}
		if !strings.Contains(r.res.Text, "output may be incomplete") {
			t.Fatalf("Execute() Text = %q, want the incomplete-output note", r.res.Text)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("Execute() did not return within 3s, want it bounded by ~2x waitDelay")
	}

	pid := readPID(t, pidFile)
	t.Cleanup(func() { killIfAlive(pid) })
}
