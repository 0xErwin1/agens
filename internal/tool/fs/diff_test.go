package fs

import "testing"

func TestRenderDiff(t *testing.T) {
	tests := []struct {
		name   string
		path   string
		before string
		after  string
		want   string
	}{
		{
			name:   "single-line change",
			path:   "a.txt",
			before: "line1\nline2\nline3\nline4\nline5\n",
			after:  "line1\nline2\nCHANGED\nline4\nline5\n",
			want: "--- a/a.txt\n" +
				"+++ b/a.txt\n" +
				"@@ -1,5 +1,5 @@\n" +
				" line1\n" +
				" line2\n" +
				"-line3\n" +
				"+CHANGED\n" +
				" line4\n" +
				" line5\n",
		},
		{
			name:   "multi-line change caps context at 3 lines",
			path:   "b.txt",
			before: "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n",
			after:  "l1\nl2\nl3\nl4\nX5\nX6\nl7\nl8\nl9\nl10\n",
			want: "--- a/b.txt\n" +
				"+++ b/b.txt\n" +
				"@@ -2,8 +2,8 @@\n" +
				" l2\n" +
				" l3\n" +
				" l4\n" +
				"-l5\n" +
				"-l6\n" +
				"+X5\n" +
				"+X6\n" +
				" l7\n" +
				" l8\n" +
				" l9\n",
		},
		{
			name:   "change at file start",
			path:   "c.txt",
			before: "OLD\nline2\nline3\nline4\n",
			after:  "NEW\nline2\nline3\nline4\n",
			want: "--- a/c.txt\n" +
				"+++ b/c.txt\n" +
				"@@ -1,4 +1,4 @@\n" +
				"-OLD\n" +
				"+NEW\n" +
				" line2\n" +
				" line3\n" +
				" line4\n",
		},
		{
			name:   "change at file end",
			path:   "d.txt",
			before: "line1\nline2\nline3\nOLD\n",
			after:  "line1\nline2\nline3\nNEW\n",
			want: "--- a/d.txt\n" +
				"+++ b/d.txt\n" +
				"@@ -1,4 +1,4 @@\n" +
				" line1\n" +
				" line2\n" +
				" line3\n" +
				"-OLD\n" +
				"+NEW\n",
		},
		{
			name:   "whole-file replacement",
			path:   "e.txt",
			before: "OLD1\nOLD2\n",
			after:  "NEW1\nNEW2\nNEW3\n",
			want: "--- a/e.txt\n" +
				"+++ b/e.txt\n" +
				"@@ -1,2 +1,3 @@\n" +
				"-OLD1\n" +
				"-OLD2\n" +
				"+NEW1\n" +
				"+NEW2\n" +
				"+NEW3\n",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := renderDiff(tt.path, tt.before, tt.after)
			if got != tt.want {
				t.Fatalf("renderDiff(%q, ...) =\n%s\nwant:\n%s", tt.path, got, tt.want)
			}
		})
	}
}
