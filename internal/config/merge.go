package config

import "fmt"

func applyPatch(cfg *Config, patch configPatch) {
	applyOptionsPatch(cfg, patch.Options)
	applyProviderPatch(cfg, patch.Provider)
	applyAgentPatch(cfg, patch.Agent)
	applyUIPatch(cfg, patch.UI)
	applyMCPPatch(cfg, patch.MCP)
}

func applyUIPatch(cfg *Config, patch *uiPatch) {
	if patch == nil {
		return
	}
	if patch.CollapseThinking != nil {
		cfg.UI.CollapseThinking = *patch.CollapseThinking
	}
	if patch.TruncateToolOutput != nil {
		cfg.UI.TruncateToolOutput = *patch.TruncateToolOutput
	}
}

func applyOptionsPatch(cfg *Config, patch *optionsPatch) {
	if patch == nil {
		return
	}
	if patch.Debug != nil {
		cfg.Options.Debug = *patch.Debug
	}
	if patch.DataDir != nil {
		cfg.Options.DataDir = *patch.DataDir
	}
}

func applyProviderPatch(cfg *Config, patch *providerPatch) {
	if patch == nil {
		return
	}
	if patch.Type != nil {
		cfg.Provider.Type = *patch.Type
	}
	if patch.Model != nil {
		cfg.Provider.Model = *patch.Model
	}
	if patch.BaseURL != nil {
		cfg.Provider.BaseURL = *patch.BaseURL
	}
}

func applyAgentPatch(cfg *Config, patch *agentPatch) {
	if patch == nil {
		return
	}
	if patch.SystemPrompt != nil {
		cfg.Agent.SystemPrompt = *patch.SystemPrompt
	}
	if patch.MaxIterations != nil {
		cfg.Agent.MaxIterations = *patch.MaxIterations
	}
	if patch.ParallelToolCalls != nil {
		cfg.Agent.ParallelToolCalls = *patch.ParallelToolCalls
	}
}

func applyMCPPatch(cfg *Config, patch map[string]mcpServerPatch) {
	if len(patch) == 0 {
		return
	}
	if cfg.MCP == nil {
		cfg.MCP = map[string]MCPServer{}
	}
	for name, serverPatch := range patch {
		server := cfg.MCP[name]
		if serverPatch.Transport != nil {
			server.Transport = *serverPatch.Transport
		}
		if serverPatch.Command != nil {
			server.Command = *serverPatch.Command
		}
		if serverPatch.Args != nil {
			server.Args = append([]string(nil), serverPatch.Args...)
		}
		if serverPatch.Env != nil {
			if server.Env == nil {
				server.Env = map[string]string{}
			}
			for key, value := range serverPatch.Env {
				server.Env[key] = value
			}
		}
		if serverPatch.CWD != nil {
			server.CWD = *serverPatch.CWD
		}
		if serverPatch.URL != nil {
			server.URL = *serverPatch.URL
		}
		if serverPatch.Headers != nil {
			if server.Headers == nil {
				server.Headers = map[string]string{}
			}
			for key, value := range serverPatch.Headers {
				server.Headers[key] = value
			}
		}
		if serverPatch.MaxRetries != nil {
			server.MaxRetries = *serverPatch.MaxRetries
		}
		cfg.MCP[name] = server
	}
}

func expandPatch(patch *configPatch, env map[string]string) error {
	if err := expandOptionsPatch(patch.Options, env); err != nil {
		return err
	}
	if err := expandProviderPatch(patch.Provider, env); err != nil {
		return err
	}
	return expandMCPPatch(patch.MCP, env)
}

func validatePatch(patch configPatch) error {
	if patch.Agent != nil && patch.Agent.MaxIterations != nil && *patch.Agent.MaxIterations < 1 {
		return fmt.Errorf("agent.max_iterations must be >= 1")
	}
	return nil
}

func validateConfig(cfg Config) error {
	for name, server := range cfg.MCP {
		prefix := fmt.Sprintf("mcp.%s", name)
		switch server.Transport {
		case "":
			return fmt.Errorf("%s.transport is required", prefix)
		case MCPTransportStdio:
			if server.Command == "" {
				return fmt.Errorf("%s.command is required for stdio transport", prefix)
			}
			if server.URL != "" || len(server.Headers) > 0 || server.MaxRetries != 0 {
				return fmt.Errorf("%s selects stdio but also sets http/sse fields", prefix)
			}
		case MCPTransportHTTP:
			if server.URL == "" {
				return fmt.Errorf("%s.url is required for http transport", prefix)
			}
			if server.Command != "" || len(server.Args) > 0 || len(server.Env) > 0 || server.CWD != "" {
				return fmt.Errorf("%s selects http but also sets stdio fields", prefix)
			}
		case MCPTransportSSE:
			if server.URL == "" {
				return fmt.Errorf("%s.url is required for sse transport", prefix)
			}
			if server.Command != "" || len(server.Args) > 0 || len(server.Env) > 0 || server.CWD != "" {
				return fmt.Errorf("%s selects sse but also sets stdio fields", prefix)
			}
			if server.MaxRetries != 0 {
				return fmt.Errorf("%s selects sse but also sets http fields", prefix)
			}
		default:
			return fmt.Errorf("%s.transport must be one of stdio, http, or sse", prefix)
		}
	}
	return nil
}

func expandOptionsPatch(patch *optionsPatch, env map[string]string) error {
	if patch == nil || patch.DataDir == nil {
		return nil
	}
	value, err := ExpandEnv(*patch.DataDir, env)
	if err != nil {
		return err
	}
	patch.DataDir = &value
	return nil
}

func expandProviderPatch(patch *providerPatch, env map[string]string) error {
	if patch == nil || patch.BaseURL == nil {
		return nil
	}
	value, err := ExpandEnv(*patch.BaseURL, env)
	if err != nil {
		return err
	}
	patch.BaseURL = &value
	return nil
}

func expandMCPPatch(patch map[string]mcpServerPatch, env map[string]string) error {
	for name, server := range patch {
		prefix := fmt.Sprintf("mcp.%s", name)
		if err := expandStringPtr(&server.Command, env); err != nil {
			return fmt.Errorf("%s.command: %w", prefix, err)
		}
		for i, arg := range server.Args {
			value, err := ExpandEnvWithCommands(arg, env)
			if err != nil {
				return fmt.Errorf("%s.args[%d]: %w", prefix, i, err)
			}
			server.Args[i] = value
		}
		if err := expandStringMap(server.Env, env, prefix+".env"); err != nil {
			return err
		}
		if err := expandStringPtr(&server.CWD, env); err != nil {
			return fmt.Errorf("%s.cwd: %w", prefix, err)
		}
		if err := expandStringPtr(&server.URL, env); err != nil {
			return fmt.Errorf("%s.url: %w", prefix, err)
		}
		if err := expandStringMap(server.Headers, env, prefix+".headers"); err != nil {
			return err
		}
		patch[name] = server
	}
	return nil
}

func expandStringPtr(value **string, env map[string]string) error {
	if *value == nil {
		return nil
	}
	expanded, err := ExpandEnvWithCommands(**value, env)
	if err != nil {
		return err
	}
	*value = &expanded
	return nil
}

func expandStringMap(values map[string]string, env map[string]string, path string) error {
	for key, value := range values {
		expanded, err := ExpandEnvWithCommands(value, env)
		if err != nil {
			return fmt.Errorf("%s.%s: %w", path, key, err)
		}
		values[key] = expanded
	}
	return nil
}
