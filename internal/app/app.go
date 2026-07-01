package app

import "github.com/iperez/agens/internal/cli"

func Run(args []string) error {
	cmd := cli.NewRootCommand()
	cmd.SetArgs(args)
	return cmd.Execute()
}
