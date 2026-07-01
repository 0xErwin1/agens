package config

import (
	"fmt"
	"strings"
	"unicode"
)

func ExpandEnv(input string, env map[string]string) (string, error) {
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
