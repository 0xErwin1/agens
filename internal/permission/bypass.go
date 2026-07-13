package permission

import "sync/atomic"

// BypassState is a live, process-local permission bypass setting.
type BypassState struct {
	enabled atomic.Bool
}

// NewBypassState returns a BypassState initialized to initial.
func NewBypassState(initial bool) *BypassState {
	state := &BypassState{}
	state.Set(initial)
	return state
}

// Enabled reports whether the bypass is active.
func (s *BypassState) Enabled() bool {
	return s.enabled.Load()
}

// Set updates the bypass setting.
func (s *BypassState) Set(enabled bool) {
	s.enabled.Store(enabled)
}
