package config

import (
	"strings"
	"testing"
)

func TestDoctorReportIsHumanReadable(t *testing.T) {
	loaded := Loaded{Config: DefaultConfig(), GlobalPath: "/global/config.toml", ProjectPath: "/repo/.agens/config.toml"}

	output := DoctorReport(loaded)
	for _, want := range []string{"Agens config doctor", "Global:", "Project:", "Status:  valid"} {
		if !strings.Contains(output, want) {
			t.Fatalf("DoctorReport() missing %q in:\n%s", want, output)
		}
	}
}
