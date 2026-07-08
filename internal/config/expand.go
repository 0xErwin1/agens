package config

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"time"
	"unicode"
)

const (
	commandSubstitutionTimeout = 2 * time.Second
	commandSubstitutionMaxOut  = 64 * 1024
)

func ExpandEnv(input string, env map[string]string) (string, error) {
	return expandEnv(input, env, false)
}

func ExpandEnvWithCommands(input string, env map[string]string) (string, error) {
	return expandEnv(input, env, true)
}

func expandEnv(input string, env map[string]string, allowCommands bool) (string, error) {
	var out strings.Builder
	for i := 0; i < len(input); {
		if input[i] != '$' {
			out.WriteByte(input[i])
			i++
			continue
		}
		if i+1 >= len(input) {
			out.WriteByte(input[i])
			i++
			continue
		}
		if input[i+1] == '{' {
			value, next, err := expandBraced(input, i, env)
			if err != nil {
				return "", err
			}
			out.WriteString(value)
			i = next
			continue
		}
		if input[i+1] == '(' && allowCommands {
			value, next, err := expandCommand(input, i)
			if err != nil {
				return "", err
			}
			out.WriteString(value)
			i = next
			continue
		}
		name, next := scanName(input, i+1)
		if name == "" {
			out.WriteByte(input[i])
			i++
			continue
		}
		value, ok := env[name]
		if !ok {
			return "", fmt.Errorf("environment variable %q is not set", name)
		}
		out.WriteString(value)
		i = next
	}
	return out.String(), nil
}

func expandBraced(input string, start int, env map[string]string) (string, int, error) {
	end := strings.IndexByte(input[start+2:], '}')
	if end < 0 {
		return "", 0, fmt.Errorf("unterminated environment expression")
	}
	end += start + 2
	expr := input[start+2 : end]
	name, fallback, hasFallback := strings.Cut(expr, ":-")
	if !validName(name) {
		return "", 0, fmt.Errorf("invalid environment variable %q", name)
	}
	value, ok := env[name]
	if ok {
		return value, end + 1, nil
	}
	if hasFallback {
		return fallback, end + 1, nil
	}
	return "", 0, fmt.Errorf("environment variable %q is not set", name)
}

func scanName(input string, start int) (string, int) {
	if start >= len(input) || !isNameStart(rune(input[start])) {
		return "", start
	}
	end := start + 1
	for end < len(input) && isNamePart(rune(input[end])) {
		end++
	}
	return input[start:end], end
}

func expandCommand(input string, start int) (string, int, error) {
	end := strings.IndexByte(input[start+2:], ')')
	if end < 0 {
		return "", 0, fmt.Errorf("unterminated command substitution")
	}
	end += start + 2
	command := input[start+2 : end]
	if strings.TrimSpace(command) == "" {
		return "", 0, fmt.Errorf("empty command substitution")
	}
	value, err := runCommandSubstitution(command)
	if err != nil {
		return "", 0, err
	}
	return value, end + 1, nil
}

func runCommandSubstitution(command string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), commandSubstitutionTimeout)
	defer cancel()

	cmd := exec.CommandContext(ctx, "sh", "-c", command)
	stdin, err := os.Open(os.DevNull)
	if err != nil {
		return "", fmt.Errorf("command substitution: open stdin: %w", err)
	}
	defer func() { _ = stdin.Close() }()
	cmd.Stdin = stdin

	output, err := cmd.CombinedOutput()
	if ctx.Err() == context.DeadlineExceeded {
		return "", fmt.Errorf("command substitution timed out after %s", commandSubstitutionTimeout)
	}
	if len(output) > commandSubstitutionMaxOut {
		return "", fmt.Errorf("command substitution output exceeds %d bytes", commandSubstitutionMaxOut)
	}
	if err != nil {
		text := strings.TrimSpace(string(output))
		if text == "" {
			return "", fmt.Errorf("command substitution failed: %w", err)
		}
		return "", fmt.Errorf("command substitution failed: %w: %s", err, text)
	}
	return strings.TrimSuffix(string(output), "\n"), nil
}

func validName(name string) bool {
	if name == "" {
		return false
	}
	for i, ch := range name {
		if i == 0 && !isNameStart(ch) {
			return false
		}
		if i > 0 && !isNamePart(ch) {
			return false
		}
	}
	return true
}

func isNameStart(ch rune) bool {
	return ch == '_' || unicode.IsLetter(ch)
}

func isNamePart(ch rune) bool {
	return isNameStart(ch) || unicode.IsDigit(ch)
}
