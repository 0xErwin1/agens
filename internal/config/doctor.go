package config

import (
	"fmt"
	"os"
	"strings"
)

func DoctorReport(loaded Loaded) string {
	var out strings.Builder
	out.WriteString("Agens config doctor\n")
	fmt.Fprintf(&out, "Global:  %s (%s)\n", loaded.GlobalPath, pathStatus(loaded.GlobalPath))
	fmt.Fprintf(&out, "Project: %s (%s)\n", loaded.ProjectPath, pathStatus(loaded.ProjectPath))
	fmt.Fprintf(&out, "MCP:     %d server(s)\n", len(loaded.Config.MCP))
	out.WriteString("Status:  valid\n")
	return out.String()
}

func DoctorErrorReport(loaded Loaded, err error) string {
	var out strings.Builder
	out.WriteString("Agens config doctor\n")
	fmt.Fprintf(&out, "Global:  %s (%s)\n", loaded.GlobalPath, pathStatus(loaded.GlobalPath))
	fmt.Fprintf(&out, "Project: %s (%s)\n", loaded.ProjectPath, pathStatus(loaded.ProjectPath))
	fmt.Fprintf(&out, "MCP:     %d server(s)\n", len(loaded.Config.MCP))
	out.WriteString("Status:  invalid\n")
	fmt.Fprintf(&out, "Error:   %v\n", err)
	return out.String()
}

func pathStatus(path string) string {
	if _, err := os.Stat(path); err == nil {
		return "loaded"
	}
	return "missing"
}
