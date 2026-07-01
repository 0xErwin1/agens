package version

var (
	Version = "dev"
	Commit  = "unknown"
	Date    = "unknown"
)

func Info() string {
	return Version
}
