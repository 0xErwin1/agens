package app

import "github.com/0xErwin1/agens/internal/cli"

func Run(args []string) error {
	cmd := cli.NewRootCommand()
	cmd.SetArgs(args)
	return cmd.Execute()
}
