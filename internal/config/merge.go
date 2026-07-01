package config

func applyPatch(cfg *Config, patch configPatch) {
	if patch.Options == nil {
		return
	}
	if patch.Options.Debug != nil {
		cfg.Options.Debug = *patch.Options.Debug
	}
	if patch.Options.DataDir != nil {
		cfg.Options.DataDir = *patch.Options.DataDir
	}
}

func expandPatch(patch *configPatch, env map[string]string) error {
	if patch.Options == nil || patch.Options.DataDir == nil {
		return nil
	}
	value, err := ExpandEnv(*patch.Options.DataDir, env)
	if err != nil {
		return err
	}
	patch.Options.DataDir = &value
	return nil
}
