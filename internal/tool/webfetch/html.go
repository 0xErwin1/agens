package webfetch

import (
	"bytes"
	"mime"
	"strings"

	"golang.org/x/net/html"
)

// blockTags emit a newline when they close, so extractText's output roughly
// preserves the page's visual paragraph/line structure instead of running
// every text node together.
var blockTags = map[string]bool{
	"p": true, "div": true, "br": true, "li": true,
	"ul": true, "ol": true,
	"h1": true, "h2": true, "h3": true, "h4": true, "h5": true, "h6": true,
	"tr": true, "table": true, "blockquote": true, "pre": true,
	"section": true, "article": true,
}

// rawTextTags hold content the tokenizer reports as a single opaque text
// node (script bodies, stylesheet rules) that is never meant for a reader
// and must be excluded from extractText's output.
var rawTextTags = map[string]bool{
	"script": true,
	"style":  true,
}

// isHTML reports whether contentType names the text/html media type,
// ignoring parameters such as charset. A Content-Type header that fails to
// parse is treated as not HTML, so the caller falls back to returning the
// body raw rather than risking a bad extraction.
func isHTML(contentType string) bool {
	mediaType, _, err := mime.ParseMediaType(contentType)
	if err != nil {
		return false
	}
	return mediaType == "text/html"
}

// extractText walks htmlBytes with a streaming tokenizer and returns its
// visible text: script/style subtrees are dropped, remaining text nodes have
// their whitespace collapsed, and block-level tags emit a newline on close so
// the result keeps rough paragraph structure. Malformed or truncated input
// does not panic — the tokenizer's ErrorToken (which also fires on a clean
// EOF) simply ends the walk and the text collected so far is returned.
func extractText(htmlBytes []byte) string {
	z := html.NewTokenizer(bytes.NewReader(htmlBytes))

	var builder strings.Builder
	var skip string

	for {
		tokenType := z.Next()

		switch tokenType {
		case html.ErrorToken:
			return strings.TrimSpace(builder.String())

		case html.TextToken:
			if skip != "" {
				continue
			}
			text := strings.Join(strings.Fields(string(z.Text())), " ")
			if text == "" {
				continue
			}
			if builder.Len() > 0 {
				builder.WriteByte(' ')
			}
			builder.WriteString(text)

		case html.StartTagToken:
			name, _ := z.TagName()
			tag := string(name)
			if rawTextTags[tag] {
				skip = tag
			}

		case html.EndTagToken, html.SelfClosingTagToken:
			name, _ := z.TagName()
			tag := string(name)
			if tag == skip {
				skip = ""
			}
			if blockTags[tag] {
				builder.WriteByte('\n')
			}
		}
	}
}
