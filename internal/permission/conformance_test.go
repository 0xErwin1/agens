package permission_test

import (
	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/permission"
)

// var _ agentloop.ToolRunner = (*permission.Gate)(nil) is the only place in
// internal/permission's test tree that imports internal/agentloop,
// verifying at compile time that *Gate structurally satisfies
// agentloop.ToolRunner without any production code depending on agentloop.
var _ agentloop.ToolRunner = (*permission.Gate)(nil)
