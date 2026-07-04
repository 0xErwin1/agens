package tui

import "testing"

func TestHumanizeError(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{
			name: "strips package tags from a turn auth error",
			in:   "agentloop: open stream: chatgpt: HTTP 401: invalid api key (invalid_api_key)",
			want: "open stream: HTTP 401: invalid api key (invalid_api_key)",
		},
		{
			name: "drops every package tag but keeps descriptive phrases",
			in:   "agentloop: open stream: chatgpt: refresh credential: chatgpt: token exchange failed: HTTP 400: non-2xx refresh response",
			want: "open stream: refresh credential: token exchange failed: HTTP 400: non-2xx refresh response",
		},
		{
			name: "openai prefix",
			in:   "openai: HTTP 500: server error (internal)",
			want: "HTTP 500: server error (internal)",
		},
		{
			name: "message without a colon is unchanged",
			in:   "stream closed unexpectedly",
			want: "stream closed unexpectedly",
		},
		{
			name: "keeps content colons that are not package tags",
			in:   "HTTP 429: rate limited: retry after 5s",
			want: "HTTP 429: rate limited: retry after 5s",
		},
		{
			name: "all-label message falls back to the original",
			in:   "chatgpt",
			want: "chatgpt",
		},
	}

	for _, c := range cases {
		if got := humanizeError(c.in); got != c.want {
			t.Fatalf("%s: humanizeError(%q) = %q, want %q", c.name, c.in, got, c.want)
		}
	}
}
