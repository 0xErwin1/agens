package config

func applyPatch(cfg *Config, patch configPatch) {
	applyOptionsPatch(cfg, patch.Options)
	applyProviderPatch(cfg, patch.Provider)
	applyAgentPatch(cfg, patch.Agent)
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
}

func expandPatch(patch *configPatch, env map[string]string) error {
	if err := expandOptionsPatch(patch.Options, env); err != nil {
		return err
	}
	return expandProviderPatch(patch.Provider, env)
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
