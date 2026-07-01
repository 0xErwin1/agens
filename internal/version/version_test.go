package version

import "testing"

func TestInfoReturnsVersion(t *testing.T) {
	original := Version
	t.Cleanup(func() { Version = original })

	Version = "test-version"
	if got := Info(); got != "test-version" {
		t.Fatalf("Info() = %q, want test-version", got)
	}
}
