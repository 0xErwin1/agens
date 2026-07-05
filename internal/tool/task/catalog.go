package task

import "sync"

// Catalog is the mutable, concurrency-safe set of selectable subagents the task
// tool advertises and validates against. The composition root builds one and
// shares it with a surface (the TUI's agents menu) so an edit to a subagent's
// allowed models takes effect for the running loop's next turn, not only for a
// fresh session. The tool reads it from the loop goroutine while the surface
// writes it from the UI goroutine, so every access is guarded.
type Catalog struct {
	mu     sync.RWMutex
	agents []Agent
}

// NewCatalog returns a Catalog seeded with a copy of agents.
func NewCatalog(agents []Agent) *Catalog {
	c := &Catalog{}
	c.Replace(agents)
	return c
}

// Replace swaps the whole set of agents for a copy of the given slice.
func (c *Catalog) Replace(agents []Agent) {
	cp := make([]Agent, len(agents))
	copy(cp, agents)

	c.mu.Lock()
	c.agents = cp
	c.mu.Unlock()
}

// Agents returns a snapshot copy of the current agents, safe for the caller to
// read without holding the lock.
func (c *Catalog) Agents() []Agent {
	c.mu.RLock()
	defer c.mu.RUnlock()

	out := make([]Agent, len(c.agents))
	copy(out, c.agents)
	return out
}

// SetModels replaces the allowed-models set of the agent named name and reports
// whether such an agent existed. The models slice is copied, so the caller may
// reuse it.
func (c *Catalog) SetModels(name string, models []string) bool {
	cp := make([]string, len(models))
	copy(cp, models)

	c.mu.Lock()
	defer c.mu.Unlock()
	for i := range c.agents {
		if c.agents[i].Name == name {
			c.agents[i].Models = cp
			return true
		}
	}
	return false
}
