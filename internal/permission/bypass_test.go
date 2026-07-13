package permission

import "testing"

func TestBypassState_DefaultsOffAndCanBeUpdated(t *testing.T) {
	state := NewBypassState(false)
	if state.Enabled() {
		t.Fatal("Enabled() = true, want false for a default-off state")
	}

	state.Set(true)
	if !state.Enabled() {
		t.Fatal("Enabled() = false after Set(true), want true")
	}

	state.Set(false)
	if state.Enabled() {
		t.Fatal("Enabled() = true after Set(false), want false")
	}
}

func TestBypassState_InstancesAreIndependent(t *testing.T) {
	first := NewBypassState(true)
	second := NewBypassState(false)

	first.Set(false)
	second.Set(true)

	if first.Enabled() {
		t.Fatal("first.Enabled() = true, want false after only first is disabled")
	}
	if !second.Enabled() {
		t.Fatal("second.Enabled() = false, want true after only second is enabled")
	}
}
