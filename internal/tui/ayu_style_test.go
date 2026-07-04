package tui

import (
	"encoding/json"
	"testing"

	"github.com/charmbracelet/glamour/ansi"
)

// TestAyuStyleJSON_CarriesAyuChromaPalette proves the embedded glamour style
// parses and highlights code with the ayu palette rather than glamour's default
// dark chroma, so code fences match the rest of the ayu theme.
func TestAyuStyleJSON_CarriesAyuChromaPalette(t *testing.T) {
	var cfg ansi.StyleConfig
	if err := json.Unmarshal(ayuStyleJSON, &cfg); err != nil {
		t.Fatalf("embedded ayu.json is not a valid glamour StyleConfig: %v", err)
	}

	chroma := cfg.CodeBlock.Chroma
	if chroma == nil {
		t.Fatal("ayu.json code_block has no chroma settings, want the ayu syntax palette")
	}

	// Spot-check the tokens visible in a typical snippet: keywords are ayu
	// orange, functions are ayu yellow, strings are ayu green, numbers purple.
	wants := map[string]struct {
		got  *string
		want string
	}{
		"keyword":        {chroma.Keyword.Color, "#FF8F40"},
		"name_function":  {chroma.NameFunction.Color, "#FFB454"},
		"literal_string": {chroma.LiteralString.Color, "#AAD94C"},
		"literal_number": {chroma.LiteralNumber.Color, "#D2A6FF"},
	}
	for token, c := range wants {
		if c.got == nil {
			t.Fatalf("chroma.%s has no color, want %s", token, c.want)
		}
		if *c.got != c.want {
			t.Fatalf("chroma.%s = %q, want the ayu color %q", token, *c.got, c.want)
		}
	}
}

func TestCodeChromaFormatter(t *testing.T) {
	cases := map[string]string{
		"truecolor": "terminal16m",
		"24bit":     "terminal16m",
		"TrueColor": "terminal16m", // case-insensitive
		"":          "terminal256",
		"256":       "terminal256",
	}
	for env, want := range cases {
		t.Setenv("COLORTERM", env)
		if got := codeChromaFormatter(); got != want {
			t.Fatalf("codeChromaFormatter() with COLORTERM=%q = %q, want %q", env, got, want)
		}
	}
}

// TestBuildRenderer_UsesEmbeddedAyuStyle proves the renderer builds from the
// embedded style (invalid JSON would leave it nil and degrade to raw text).
func TestBuildRenderer_UsesEmbeddedAyuStyle(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	if m.renderer == nil {
		t.Fatal("renderer is nil after SetSize, want a renderer built from the embedded ayu style")
	}
}
