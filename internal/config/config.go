package config

import (
	"os"
	"path/filepath"
)

const AppName = "agens"

func HomeDir() string {
	if value := os.Getenv("AGENS_CONFIG_HOME"); value != "" {
		return value
	}
	if value := os.Getenv("XDG_CONFIG_HOME"); value != "" {
		return filepath.Join(value, AppName)
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(".", ".agens")
	}
	return filepath.Join(home, ".config", AppName)
}
