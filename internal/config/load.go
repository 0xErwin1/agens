package config

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/pelletier/go-toml/v2"
)

type Scope string

const (
	ScopeGlobal  Scope = "global"
	ScopeProject Scope = "project"
)

type Config struct {
	Options Options `toml:"options"`
}

type Options struct {
	Debug   bool   `toml:"debug"`
	DataDir string `toml:"data_dir"`
}

type Source struct {
	Path  string
	Scope Scope
}

type Loaded struct {
	Config      Config
	Sources     []Source
	GlobalPath  string
	ProjectPath string
	ProjectRoot string
}

type LoadOptions struct {
	ConfigHome string
	WorkingDir string
	Env        map[string]string
}

type configPatch struct {
	Options *optionsPatch `toml:"options"`
}

type optionsPatch struct {
	Debug   *bool   `toml:"debug"`
	DataDir *string `toml:"data_dir"`
}

func DefaultConfig() Config {
	return Config{
		Options: Options{
			Debug:   false,
			DataDir: filepath.Join(defaultDataHome(), AppName),
		},
	}
}

func Load() (Loaded, error) {
	cwd, err := os.Getwd()
	if err != nil {
		return Loaded{}, fmt.Errorf("get working directory: %w", err)
	}
	return LoadFrom(LoadOptions{ConfigHome: HomeDir(), WorkingDir: cwd, Env: environMap()})
}

func LoadFrom(opts LoadOptions) (Loaded, error) {
	configHome := opts.ConfigHome
	if configHome == "" {
		configHome = HomeDir()
	}
	workingDir := opts.WorkingDir
	if workingDir == "" {
		cwd, err := os.Getwd()
		if err != nil {
			return Loaded{}, fmt.Errorf("get working directory: %w", err)
		}
		workingDir = cwd
	}
	env := opts.Env
	if env == nil {
		env = environMap()
	}

	projectRoot := ProjectRoot(workingDir)
	loaded := Loaded{
		Config:      DefaultConfig(),
		GlobalPath:  filepath.Join(configHome, "config.toml"),
		ProjectRoot: projectRoot,
		ProjectPath: filepath.Join(projectRoot, ".agens", "config.toml"),
	}

	if err := loaded.applyFile(loaded.GlobalPath, ScopeGlobal, env); err != nil {
		return Loaded{}, err
	}
	if err := loaded.applyFile(loaded.ProjectPath, ScopeProject, env); err != nil {
		return Loaded{}, err
	}
	return loaded, nil
}

func ProjectRoot(start string) string {
	current, err := filepath.Abs(start)
	if err != nil {
		return start
	}
	info, err := os.Stat(current)
	if err == nil && !info.IsDir() {
		current = filepath.Dir(current)
	}
	for {
		if _, err := os.Stat(filepath.Join(current, ".git")); err == nil {
			return current
		}
		next := filepath.Dir(current)
		if next == current {
			return start
		}
		current = next
	}
}

func (l *Loaded) applyFile(path string, scope Scope, env map[string]string) error {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("%s config %s: read: %w", scope, path, err)
	}

	patch, err := decodePatch(data)
	if err != nil {
		return fmt.Errorf("%s config %s: %w", scope, path, err)
	}
	if err := expandPatch(&patch, env); err != nil {
		return fmt.Errorf("%s config %s: %w", scope, path, err)
	}
	applyPatch(&l.Config, patch)
	l.Sources = append(l.Sources, Source{Path: path, Scope: scope})
	return nil
}

func decodePatch(data []byte) (configPatch, error) {
	var patch configPatch
	decoder := toml.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&patch); err != nil {
		return configPatch{}, fmt.Errorf("invalid TOML: %w", err)
	}
	return patch, nil
}

func environMap() map[string]string {
	result := make(map[string]string)
	for _, item := range os.Environ() {
		key, value, ok := splitEnv(item)
		if ok {
			result[key] = value
		}
	}
	return result
}

func splitEnv(item string) (string, string, bool) {
	for i, ch := range item {
		if ch == '=' {
			return item[:i], item[i+1:], true
		}
	}
	return "", "", false
}

func defaultDataHome() string {
	if value := os.Getenv("XDG_DATA_HOME"); value != "" {
		return value
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(".", ".local", "share")
	}
	return filepath.Join(home, ".local", "share")
}
