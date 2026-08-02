use agens_core::{SubagentErrorKind, SubagentStatus};
use std::time::Duration;

use agens_core::ask_user::{AskUserMode, AskUserOption, AskUserQuestion, AskUserRequest};
use agens_core::{Message, MessagePart, Role, TurnEvent, TurnRetryReason, Usage};
use agens_tui::{
    Action, ColorLevel, ConversationEvent, DialogEntry, DialogView, DiffLine, DiffLineKind,
    DisplayMode, Engine, Event, Key, PaletteEntry, PaletteEntryKind, RatatuiRenderer, Renderer,
    RepositoryStatus, SessionDialogCursor, SessionDialogRequest, ToolResultState, TranscriptId,
    Tui, TuiExecutionEvent, TuiExecutionState, TuiPresentation, TuiRuntimeEvent, TuiSubagentEvent,
    TuiSubmissionOutcome, UnicodeLevel,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};

#[derive(Default)]
struct FakeEngine;

impl Engine for FakeEngine {
    fn cancel(&mut self) {}
}

/// What the terminal shows, with the hyperlink sequences taken back out.
///
/// A cell may carry an OSC 8 payload it never displays, so a test asserting on
/// what the reader sees has to read past it. Tests about the links themselves
/// use [`rendered_raw`].
fn rendered_text(renderer: &RatatuiRenderer<TestBackend>) -> String {
    strip_osc8(&rendered_raw(renderer))
}

fn rendered_raw(renderer: &RatatuiRenderer<TestBackend>) -> String {
    renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn strip_osc8(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("\u{1b}]8;;") {
        stripped.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find("\u{1b}\\") {
            Some(end) => rest = &after[end + 2..],
            None => return stripped,
        }
    }
    stripped.push_str(rest);
    stripped
}

fn apply_subagent(tui: &mut Tui<FakeEngine>, event: TuiSubagentEvent) {
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(event));
}

fn cell_for_text<'a>(
    renderer: &'a RatatuiRenderer<TestBackend>,
    text: &str,
) -> &'a ratatui::buffer::Cell {
    let index = cell_index(renderer, text);
    &renderer.terminal().backend().buffer().content[index]
}

fn rendered_row(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    cell_index(renderer, text) / usize::from(renderer.terminal().backend().buffer().area.width)
}

fn rendered_line(renderer: &RatatuiRenderer<TestBackend>, row: usize) -> String {
    let buffer = renderer.terminal().backend().buffer();
    let width = usize::from(buffer.area.width);
    let raw: String = buffer.content[row * width..(row + 1) * width]
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    strip_osc8(&raw)
}

fn rendered_column(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    cell_index(renderer, text) % usize::from(renderer.terminal().backend().buffer().area.width)
}

/// Columns of breathing room the bottom chrome keeps on both sides on terminals
/// wide enough to afford it. Every helper below assumes such a width.
const CHROME_GUTTER: u16 = 4;

fn composer_top_row(renderer: &RatatuiRenderer<TestBackend>) -> u16 {
    let buffer = renderer.terminal().backend().buffer();
    (0..buffer.area.height)
        .find(|row| buffer[(CHROME_GUTTER, *row)].symbol() == "┌")
        .expect("composer top border should be rendered")
}

fn composer_bottom_row(renderer: &RatatuiRenderer<TestBackend>) -> u16 {
    let buffer = renderer.terminal().backend().buffer();
    (0..buffer.area.height)
        .find(|row| buffer[(CHROME_GUTTER, *row)].symbol() == "└")
        .expect("composer bottom border should be rendered")
}

fn start_execution(tui: &mut Tui<FakeEngine>, id: u64, agent: &str) {
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: agent.into(),
        event: TuiExecutionEvent::ForegroundStarted { id },
    });
    apply_subagent(
        tui,
        TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
    );
}

fn cell_index(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    let buffer = renderer.terminal().backend().buffer();
    let width = text.chars().count();
    buffer
        .content
        .windows(width)
        .position(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>() == text)
        .expect("text should be rendered")
}

#[test]
fn transcript_drag_selection_paints_exact_cells_and_preserves_original_text() {
    let terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("prompt");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "alpha café 🙂 omega".into(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    renderer.render(tui.view()).unwrap();

    let row = rendered_row(&renderer, "café") as u16;
    let column = rendered_column(&renderer, "café") as u16;
    tui.handle(Event::MouseDown { column, row });
    tui.handle(Event::MouseDrag {
        column: column + 3,
        row,
    });
    tui.handle(Event::MouseUp {
        column: column + 3,
        row,
    });
    renderer.render(tui.view()).unwrap();

    assert_eq!(tui.selected_text(), Some("café"));
    for offset in 0..4 {
        let cell = &renderer.terminal().backend().buffer()[(column + offset, row)];
        assert_eq!(cell.bg, Color::Rgb(0x1b, 0x33, 0x30));
        assert_eq!(
            cell.fg,
            Color::Rgb(0xd6, 0xd4, 0xcd),
            "selected text must take the selection foreground; leaving the \
             original colour under a selection wash is how it became unreadable"
        );
    }
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("alpha café"), "{rendered:?}");
    assert!(rendered.contains("omega"), "{rendered:?}");
}

/// Wrapping is a fact about the terminal, not about the text. A paragraph that
/// happened to need three rows must come back off the clipboard as the one line
/// it was, or every quoted answer arrives with the viewport's width baked into
/// it.
#[test]
fn copying_a_wrapped_paragraph_reconstructs_the_logical_line() {
    let paragraph = "ALPHA the assistant wrote one continuous paragraph that has to \
survive being folded across several terminal rows before it reaches OMEGA";
    let terminal = Terminal::new(TestBackend::new(48, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 48,
        height: 24,
    });
    tui.begin_submission("prompt");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        paragraph.to_owned(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    renderer.render(tui.view()).unwrap();

    let first_row = rendered_row(&renderer, "ALPHA") as u16;
    let first_column = rendered_column(&renderer, "ALPHA") as u16;
    let last_row = rendered_row(&renderer, "OMEGA") as u16;
    let last_column = rendered_column(&renderer, "OMEGA") as u16 + 4;
    assert!(last_row > first_row, "the paragraph has to actually wrap");

    tui.handle(Event::MouseDown {
        column: first_column,
        row: first_row,
    });
    tui.handle(Event::MouseDrag {
        column: last_column,
        row: last_row,
    });
    tui.handle(Event::MouseUp {
        column: last_column,
        row: last_row,
    });

    assert_eq!(tui.selected_text(), Some(paragraph));
}

/// The seam inside a word carries no separator, so rejoining must not invent
/// one. A path broken across rows has to come back as one path, not two.
#[test]
fn copying_a_word_broken_mid_token_does_not_invent_a_separator() {
    let token = format!("ALPHA{}OMEGA", "x".repeat(45));
    let token = token.as_str();
    let terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 40,
        height: 24,
    });
    tui.begin_submission("prompt");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(token.to_owned())));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    renderer.render(tui.view()).unwrap();

    let first_row = rendered_row(&renderer, "ALPHA") as u16;
    let first_column = rendered_column(&renderer, "ALPHA") as u16;
    let last_row = rendered_row(&renderer, "OMEGA") as u16;
    let last_column = rendered_column(&renderer, "OMEGA") as u16 + 4;
    assert!(
        last_row > first_row,
        "the token has to actually break mid-word: {:?}",
        rendered_text(&renderer)
    );

    tui.handle(Event::MouseDown {
        column: first_column,
        row: first_row,
    });
    tui.handle(Event::MouseDrag {
        column: last_column,
        row: last_row,
    });
    tui.handle(Event::MouseUp {
        column: last_column,
        row: last_row,
    });

    assert_eq!(tui.selected_text(), Some(token));
}

/// Folding old work is only worth a row when it hides more than it costs, and
/// the fold has to be reversible from the row itself — a reader who scrolls to
/// the top has to find the way back, not a dead end.
#[test]
fn a_long_transcript_folds_its_settled_turns_behind_a_reversible_count() {
    let turns = |count: usize| {
        (0..count)
            .flat_map(|turn| {
                [
                    Message {
                        role: Role::User,
                        parts: vec![MessagePart::Text(format!("user-{turn:02}"))],
                    },
                    Message {
                        role: Role::Assistant,
                        parts: vec![MessagePart::Text(format!("answer-{turn:02}"))],
                    },
                ]
            })
            .collect::<Vec<_>>()
    };

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(60, 60)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 60,
        height: 60,
    });

    // Seven settled turns is one over the visible window: folding a single turn
    // would trade one row for one row, so nothing is folded.
    tui.replace_history(&turns(7)).unwrap();
    renderer.render(tui.view()).unwrap();
    let short = rendered_text(&renderer);
    assert!(short.contains("user-00"), "{short:?}");
    assert!(!short.contains("earlier turn"), "{short:?}");

    tui.replace_history(&turns(11)).unwrap();
    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    let folded = rendered_text(&renderer);
    assert!(
        folded.contains("… 5 earlier turns · ^Y to show"),
        "{folded:?}"
    );
    assert!(!folded.contains("user-00"), "{folded:?}");
    assert!(
        folded.contains("user-10"),
        "the recent turns stay: {folded:?}"
    );

    tui.handle(Event::Key(Key::CtrlY));
    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    let unfolded = rendered_text(&renderer);
    assert!(unfolded.contains("user-00"), "{unfolded:?}");
    assert!(!unfolded.contains("earlier turns"), "{unfolded:?}");

    tui.handle(Event::Key(Key::CtrlY));
    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("… 5 earlier turns"),
        "the fold closes again"
    );
}

/// A path the agent touched is the thing the reader most often wants to open,
/// and a link is only worth having if it costs the text nothing: the row must
/// read exactly the same with the sequence spliced in as without it.
#[test]
fn a_path_in_a_tool_row_becomes_an_openable_link_without_changing_the_row() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 100,
        height: 24,
    });
    tui.set_hyperlinks(true);
    tui.set_project("/home/iperez/dev/personal/agens");
    tui.begin_submission("inspect");
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "native::read".into(),
        input: "crates/agens-tui/src/lib.rs".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-1".into(),
        content: "body".into(),
        is_error: false,
    }));
    renderer.render(tui.view()).unwrap();

    let raw = rendered_raw(&renderer);
    assert!(
        raw.contains(
            "\u{1b}]8;;file:///home/iperez/dev/personal/agens/crates/agens-tui/src/lib.rs\u{1b}\\"
        ),
        "{raw:?}"
    );
    assert!(
        raw.contains("\u{1b}]8;;\u{1b}\\"),
        "the link closes: {raw:?}"
    );

    let visible = rendered_text(&renderer);
    assert!(
        visible.contains("read crates/agens-tui/src/lib.rs"),
        "{visible:?}"
    );
    assert!(!visible.contains('\u{1b}'), "{visible:?}");
}

/// A palette that resolves to one undifferentiated colour, or chrome that
/// resolves to replacement characters, is not a degraded transcript — it is an
/// unreadable one. Both fallbacks are judged by what survives them: the
/// distinctions, and the column each glyph occupies.
#[test]
fn the_transcript_stays_legible_on_sixteen_colours_and_without_extended_glyphs() {
    let render = |color: ColorLevel, unicode: UnicodeLevel| {
        let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 24)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 24,
        });
        tui.set_capabilities(color, unicode);
        tui.begin_submission("request");
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
            "assistant body".into(),
        )));
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: "read-1".into(),
            name: "native::read".into(),
            input: "src/lib.rs".into(),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "read-1".into(),
            content: "body".into(),
            is_error: false,
        }));
        tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));
        renderer.render(tui.view()).unwrap();
        renderer
    };

    let truecolor = render(ColorLevel::TrueColor, UnicodeLevel::Extended);
    let colours_of = |renderer: &RatatuiRenderer<TestBackend>| {
        let mut seen = Vec::new();
        for cell in &renderer.terminal().backend().buffer().content {
            if !seen.contains(&cell.fg) {
                seen.push(cell.fg);
            }
        }
        seen
    };
    let rich = colours_of(&truecolor).len();

    for level in [ColorLevel::Ansi256, ColorLevel::Ansi16] {
        let renderer = render(level, UnicodeLevel::Extended);
        let seen = colours_of(&renderer);
        assert!(
            seen.iter().all(|colour| !matches!(colour, Color::Rgb(..))),
            "{level:?} still sends 24-bit colour: {seen:?}"
        );
        assert!(
            seen.len() * 2 >= rich,
            "{level:?} collapsed {rich} distinctions into {}",
            seen.len()
        );
    }

    let plain = render(ColorLevel::None, UnicodeLevel::Extended);
    assert!(
        colours_of(&plain)
            .iter()
            .all(|colour| *colour == Color::Reset),
        "the terminal's own colours are the whole palette here"
    );

    // The ASCII transcript says the same things in the same columns.
    let extended = render(ColorLevel::TrueColor, UnicodeLevel::Extended);
    let ascii = render(ColorLevel::TrueColor, UnicodeLevel::Ascii);
    let text = rendered_text(&ascii);
    assert!(text.contains("assistant body"), "{text:?}");
    assert!(text.contains("read src/lib.rs"), "{text:?}");
    for glyph in ['┃', '◆', '❯', '●'] {
        assert!(
            !text.contains(glyph),
            "{glyph} survived the fallback: {text:?}"
        );
    }
    assert_eq!(
        rendered_column(&ascii, "assistant body"),
        rendered_column(&extended, "assistant body"),
        "the content column does not move with the locale"
    );
}

/// Hover is an accelerator over the keyboard path, never a second one. It sets
/// the focus `j`/`k` set and opens nothing by itself, so a session with mouse
/// capture off loses speed and no capability.
#[test]
fn hovering_a_block_focuses_it_and_adds_nothing_the_keyboard_cannot_do() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 80,
        height: 24,
    });
    tui.begin_submission("request");
    for id in ["read-1", "read-2"] {
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: id.into(),
            name: "native::read".into(),
            input: format!("{id}.log"),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: id.into(),
            content: format!("body of {id}"),
            is_error: false,
        }));
    }
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));
    renderer.render(tui.view()).unwrap();

    assert_eq!(tui.view().focused_call, None);

    let second = rendered_row(&renderer, "read-2.log") as u16;
    tui.handle(Event::MouseMove {
        column: 10,
        row: second,
    });
    assert_eq!(tui.view().focused_call, Some("read-2"));
    assert!(
        !tui.view().tool_display_modes.contains_key("read-2")
            || tui.view().tool_display_modes.get("read-2") == Some(&DisplayMode::Collapsed),
        "hover moves focus; opening still costs a deliberate press"
    );

    // Movement that lands on the block already under the cursor changes
    // nothing a reader could see, and must not cost a repaint: pointer events
    // arrive dozens of times per second and almost all of them are this one.
    assert_eq!(
        tui.handle(Event::MouseMove {
            column: 14,
            row: second,
        }),
        Action::Unchanged
    );

    let first = rendered_row(&renderer, "read-1.log") as u16;
    assert_eq!(
        tui.handle(Event::MouseMove {
            column: 10,
            row: first,
        }),
        Action::Render
    );
    assert_eq!(tui.view().focused_call, Some("read-1"));

    // Off the transcript there is nothing to focus, and nothing is claimed.
    tui.handle(Event::MouseMove { column: 10, row: 0 });
    assert_eq!(tui.view().focused_call, Some("read-1"));

    // Every capability hover reaches is one the keyboard already had.
    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Char('o')));
    assert_eq!(
        tui.view().tool_display_modes.get("read-1"),
        Some(&DisplayMode::Truncated)
    );
}

#[test]
fn empty_composer_renders_a_complete_dock() {
    let width = 72_u16;
    let height = 14_u16;
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let tui = Tui::new(FakeEngine);

    renderer.render(tui.view()).unwrap();

    let buffer = renderer.terminal().backend().buffer();
    let composer_top = height - 6;
    let composer_bottom = height - 4;
    assert_eq!(buffer[(CHROME_GUTTER, composer_top)].symbol(), "┌");
    assert_eq!(
        buffer[(width - 1 - CHROME_GUTTER, composer_top)].symbol(),
        "┐"
    );
    assert_eq!(buffer[(CHROME_GUTTER, composer_bottom)].symbol(), "└");
    assert_eq!(
        buffer[(width - 1 - CHROME_GUTTER, composer_bottom)].symbol(),
        "┘"
    );
}

#[test]
fn bottom_chrome_bands_share_one_gutter_and_the_composer_keeps_both_edges_free() {
    let (width, height) = (120_u16, 24_u16);
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    tui.add_info("notice-sentinel");
    start_execution(&mut tui, 9, "explore");

    renderer.render(tui.view()).unwrap();
    let top = composer_top_row(&renderer);
    let bottom = composer_bottom_row(&renderer);
    let buffer = renderer.terminal().backend().buffer();

    assert_eq!(buffer[(CHROME_GUTTER, top)].symbol(), "┌");
    assert_eq!(buffer[(width - 1 - CHROME_GUTTER, top)].symbol(), "┐");
    assert_eq!(buffer[(CHROME_GUTTER, bottom)].symbol(), "└");
    assert_eq!(buffer[(width - 1 - CHROME_GUTTER, bottom)].symbol(), "┘");
    for row in top..=bottom {
        for column in 0..CHROME_GUTTER {
            assert_eq!(
                buffer[(column, row)].symbol(),
                " ",
                "left gutter must stay blank at column {column} row {row}"
            );
            assert_eq!(
                buffer[(width - 1 - column, row)].symbol(),
                " ",
                "right gutter must stay blank at column {} row {row}",
                width - 1 - column
            );
        }
    }

    assert_eq!(
        rendered_column(&renderer, "Tab focus"),
        usize::from(CHROME_GUTTER),
        "the subagent tree starts at the shared gutter"
    );
    assert_eq!(
        rendered_column(&renderer, "notice-sentinel"),
        usize::from(CHROME_GUTTER + 1),
        "the notice starts at the shared gutter plus its own leading space"
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        bottom,
        "the metadata rides the composer's bottom border"
    );
    assert_eq!(
        buffer[(width - 2 - CHROME_GUTTER, bottom)].symbol(),
        " ",
        "the metadata stops one column short of the closing corner"
    );
}

#[test]
fn responsive_layout_saturates_heights_one_through_six() {
    for height in 1_u16..=12 {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(40, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Key(Key::Char('x')));

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        assert_eq!(
            text.chars().count(),
            40 * usize::from(height),
            "height {height}"
        );
        assert!(!text.contains("Compose"), "height {height}: {text:?}");
        assert!(!text.contains("agens safe"), "height {height}: {text:?}");
        if height >= 12 {
            assert!(
                text.contains("Ready") || text.contains("gpt"),
                "height {height}: expected footer metrics: {text:?}"
            );
        }
    }

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(40, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for character in (0..10)
        .map(|line| format!("line-{line:02}"))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
    {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    // Multiline composer input is still present without the old "N lines" chrome.
    assert!(text.contains("line-04"), "{text:?}");
    assert!(text.contains("line-09"), "{text:?}");
    assert!(!text.contains("10 lines"), "{text:?}");
}

#[test]
fn conversational_surface_uses_full_width_and_moves_context_to_footer() {
    for width in [80_u16, 100, 120] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, 12)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.set_presentation("openai-api", "gpt-4.1", "session #42");
        tui.set_project("/home/iperez/dev/personal/agens");
        tui.set_reasoning_effort(Some("high"));
        tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
            input_tokens: Some(3),
            output_tokens: Some(5),
            total_tokens: Some(8),
            context_window: Some(128),
        }));

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        // Header no longer carries product/model/project chrome.
        assert!(!text.contains("agens safe"), "{text:?}");
        assert!(!text.contains("openai-api / gpt-4.1"), "{text:?}");
        if width == 120 {
            assert!(text.contains("gpt-4.1"), "footer model: {text:?}");
            assert!(text.contains("high"), "footer effort: {text:?}");
            assert!(text.contains("agens"), "footer project basename: {text:?}");
            assert!(text.contains("8/128"), "footer usage: {text:?}");
            assert!(text.contains("6%"), "footer context share: {text:?}");
            assert!(text.contains("Ready"), "{text:?}");
            assert!(!text.contains("Enter send"), "{text:?}");
        }
    }
}

#[test]
fn footer_shows_compact_tokens_used_over_window_without_header_ctx() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.set_presentation("openai-api", "gpt-4.1", "session #1");
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        context_window: Some(8_192),
    }));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("15/8.2k"), "{text:?}");
    assert!(text.contains("0%"), "{text:?}");
    assert!(!text.contains("ctx 15/8192"), "{text:?}");
    assert!(!text.contains("context 8192"), "{text:?}");
    assert!(!text.contains("unavailable"), "{text:?}");
}

#[test]
fn footer_keeps_five_fields_and_usage_across_submission_start() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_presentation(
        TuiPresentation::new("openai-api", "gpt-4.1", "session #1")
            .with_effort("high")
            .with_context_window(Some(200_000)),
    );
    tui.set_project("/home/iperez/dev/personal/agens");

    renderer.render(tui.view()).unwrap();
    let before_usage = rendered_text(&renderer);
    assert!(
        before_usage.contains(
            "gpt-4.1 · high ·    0/200k   0% · tools hidden ^O · ~/d/p/agens · ask ^⇧P · Ready"
        ),
        "{before_usage:?}"
    );
    assert!(!before_usage.contains("model · default · ctx —"));

    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(70_000),
        output_tokens: Some(1_000),
        total_tokens: Some(71_000),
        context_window: None,
    }));
    tui.begin_submission("next turn");

    assert_eq!(
        tui.view().latest_usage.and_then(|usage| usage.total_tokens),
        Some(71_000)
    );
    renderer.render(tui.view()).unwrap();
    let next_turn = rendered_text(&renderer);
    assert!(
        next_turn.contains("gpt-4.1 · high ·  71k/200k  36% · tools hidden ^O · ~/d/p/agens"),
        "{next_turn:?}"
    );
}

#[test]
fn footer_uses_explicit_fallbacks_without_inventing_values() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let tui = Tui::new(FakeEngine);

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("model — · effort — · ctx —"), "{text:?}");
    assert!(!text.contains("model · default · ctx —"), "{text:?}");
}

#[test]
fn active_status_glyph_advances_with_tick_and_idle_stays_static() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);

    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let idle_early = rendered_text(&renderer);
    tui.tick(Duration::from_millis(80 * 5));
    renderer.render(tui.view()).unwrap();
    let idle_later = rendered_text(&renderer);

    assert!(idle_early.contains("Ready"), "{idle_early:?}");
    assert!(idle_later.contains("Ready"), "{idle_later:?}");
    assert!(!idle_early.contains("⠋"), "{idle_early:?}");
    assert!(!idle_later.contains("⠼"), "{idle_later:?}");
    assert_eq!(
        idle_early.matches('·').count(),
        idle_later.matches('·').count(),
        "idle chrome must not spin"
    );

    tui.begin_submission("glyph probe");
    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let active_early = rendered_text(&renderer);
    tui.tick(Duration::from_millis(80));
    renderer.render(tui.view()).unwrap();
    let active_later = rendered_text(&renderer);

    assert!(active_early.contains("⠋"), "{active_early:?}");
    assert!(
        active_early.contains("Waiting for the model…"),
        "{active_early:?}"
    );
    assert!(active_later.contains("⠙"), "{active_later:?}");
    assert!(
        active_later.contains("Waiting for the model…"),
        "{active_later:?}"
    );
    assert_ne!(active_early, active_later);
}

#[test]
fn working_indicator_remains_visible_when_live_transcript_reaches_the_composer() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("fill the viewport");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        (0..20)
            .map(|line| format!("output-line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("output-line-14"), "{rendered:?}");
    assert!(rendered.contains("output-line-19"), "{rendered:?}");
    assert!(rendered.contains("Responding…"), "{rendered:?}");
    assert!(!rendered.contains("LIVE"), "{rendered:?}");
    assert!(!rendered.contains("SCROLL"), "{rendered:?}");
}

#[test]
fn a_provider_backoff_reads_differently_from_an_ordinary_wait() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("ask");

    renderer.render(tui.view()).unwrap();
    let waiting = rendered_text(&renderer);

    tui.apply_progress(TurnEvent::ProviderRetry {
        attempt: 2,
        max_attempts: Some(3),
        delay: Some(Duration::from_millis(1500)),
        reason: TurnRetryReason::RateLimited,
    });
    renderer.render(tui.view()).unwrap();
    let retrying = rendered_text(&renderer);

    assert!(waiting.contains("Waiting for the model…"), "{waiting:?}");
    assert!(!waiting.contains("Retrying"), "{waiting:?}");
    assert!(retrying.contains("Retrying (2/3)"), "{retrying:?}");
    assert!(retrying.contains("rate limited"), "{retrying:?}");
    assert!(retrying.contains("retrying in 1.5s"), "{retrying:?}");
}

#[test]
fn a_retry_stops_being_reported_once_the_next_attempt_produces_output() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("ask");
    tui.apply_progress(TurnEvent::ProviderRetry {
        attempt: 1,
        max_attempts: Some(3),
        delay: Some(Duration::from_millis(250)),
        reason: TurnRetryReason::ServerError,
    });
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(!rendered.contains("Retrying"), "{rendered:?}");
    assert!(rendered.contains("Responding…"), "{rendered:?}");
}

#[test]
fn a_settled_turn_keeps_what_it_took_and_what_it_billed() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("ask");
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(1_200),
        output_tokens: Some(300),
        total_tokens: Some(1_500),
        context_window: Some(200_000),
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(2_000),
        output_tokens: Some(700),
        total_tokens: Some(2_700),
        context_window: Some(200_000),
    }));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));
    tui.tick(Duration::from_secs(14));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("14s"), "{rendered:?}");
    assert!(rendered.contains("3.2k tok in"), "{rendered:?}");
    assert!(rendered.contains("1.0k tok out"), "{rendered:?}");
}

#[test]
fn a_reasoning_stretch_reports_how_long_it_has_been_running() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("ask");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "weighing options".into(),
    )));
    tui.tick(Duration::from_secs(9));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("Reasoning… 9s"), "{rendered:?}");
}

#[test]
fn footer_shows_tokens_used_only_when_window_unknown_without_unavailable() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(4),
        output_tokens: Some(6),
        total_tokens: Some(10),
        context_window: None,
    }));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("10"), "{text:?}");
    assert!(!text.contains("10/"), "{text:?}");
    assert!(!text.contains("unavailable"), "{text:?}");
    assert!(!text.contains("context "), "{text:?}");
}

#[test]
fn typed_turn_blocks_group_tools_with_status_duration_and_preview() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 100)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect the workspace");

    for event in [
        ConversationEvent::ReasoningDelta("inspect typed events".into()),
        ConversationEvent::ToolCall {
            call_id: "read-1".into(),
            name: "native::read".into(),
            input: "src/lib.rs".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::read".into(),
                raw: "src/lib.rs".into(),
            },
        },
        ConversationEvent::ToolCall {
            call_id: "grep-2".into(),
            name: "native::grep".into(),
            input: "needle".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::grep".into(),
                raw: "needle".into(),
            },
        },
        ConversationEvent::ToolResult {
            call_id: "grep-2".into(),
            output: "grep preview".into(),
            is_error: true,
        },
        ConversationEvent::ToolResult {
            call_id: "read-1".into(),
            output: format!("read preview\n{}", "long-output ".repeat(400)),
            is_error: false,
        },
        ConversationEvent::Error {
            message: "safe failure".into(),
            action: "retry".into(),
        },
    ] {
        tui.apply_conversation_event(event).unwrap();
    }
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "read-1".into(),
        duration: Some(Duration::from_millis(12)),
        result: ToolResultState::Success,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "grep-2".into(),
        duration: Some(Duration::from_secs(2)),
        result: ToolResultState::Failure,
    });
    tui.handle(Event::Key(Key::CtrlO));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert_eq!(text.matches("Tools · batch 1").count(), 1, "{text:?}");
    for expected in [
        "inspect the workspace",
        "inspect typed events",
        "src/lib.rs",
        "needle",
        "read preview",
        "grep preview",
        "Success · 12ms",
        "Failure · 2s",
        "visible output truncated",
        "safe failure",
        "Action: retry",
    ] {
        assert_eq!(text.matches(expected).count(), 1, "{expected}: {text:?}");
    }
    assert!(text.contains("read src/lib.rs"), "{text:?}");
    assert!(text.contains("grep needle"), "{text:?}");
    assert!(
        text.find("grep preview") < text.find("read preview"),
        "out-of-order results must keep arrival order: {text:?}"
    );
    assert!(!text.contains("└ read"), "{text:?}");
    assert!(!text.contains("└ grep"), "{text:?}");
    assert!(!text.contains("native::read · read-1"), "{text:?}");
    assert!(!text.contains("native::grep · grep-2"), "{text:?}");
}

#[test]
fn composer_dock_and_footer_degrade_without_detached_bands() {
    for (height, expects_footer, composer_y) in [(10, false, 7), (12, true, 8)] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Key(Key::Char('d')));
        tui.add_info("compact-status-sentinel");

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        assert_eq!(
            text.matches("compact-status-sentinel").count(),
            1,
            "height {height}: {text:?}"
        );
        if expects_footer {
            assert!(
                text.contains("Ready") || text.contains("Responding"),
                "height {height}: expected operational footer: {text:?}"
            );
        }
        let _ = composer_y; // Compose title removed
        assert!(!text.contains("Compose"), "{text:?}");
        assert!(!text.contains("Enter send"), "{text:?}");
        assert!(!text.contains("F5"), "{text:?}");
        assert!(!text.contains("F6"), "{text:?}");
        assert!(!text.contains("TURN"), "{text:?}");
        assert!(!text.contains("USAGE"), "{text:?}");
    }
}

#[test]
fn multiline_wrapped_user_message_uses_one_accented_identity() {
    let terminal = Terminal::new(TestBackend::new(44, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission(
        "A deliberately long user message wraps naturally in this narrow viewport.\nSecond source line.",
    );
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains('❯'), "{text:?}");
    assert!(!text.contains("USER"), "{text:?}");
    assert!(text.contains("deliberately long user"), "{text:?}");
    assert!(text.contains("Second source line."), "{text:?}");
    let user = cell_for_text(&renderer, "❯");
    assert_eq!(user.fg, Color::Rgb(0xd2, 0xa6, 0xff));
    assert!(user.modifier.contains(Modifier::BOLD));
}

#[test]
fn user_turns_have_a_distinct_identity_rail_and_compact_separation() {
    let terminal = Terminal::new(TestBackend::new(72, 30)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("USER_FIRST_SENTINEL");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "AGENT_FIRST_SENTINEL\n\nAGENT_SECOND_SENTINEL".into(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    tui.begin_submission("USER_SECOND_SENTINEL\nUSER_CONTINUATION_SENTINEL");
    renderer.render(tui.view()).unwrap();

    let first_user_row = rendered_row(&renderer, "USER_FIRST_SENTINEL");
    let first_agent_row = rendered_row(&renderer, "AGENT_FIRST_SENTINEL");
    let second_agent_row = rendered_row(&renderer, "AGENT_SECOND_SENTINEL");
    let second_user_row = rendered_row(&renderer, "USER_SECOND_SENTINEL");
    let continuation_row = rendered_row(&renderer, "USER_CONTINUATION_SENTINEL");
    let buffer = renderer.terminal().backend().buffer();

    for row in [first_user_row, second_user_row, continuation_row] {
        assert_eq!(buffer[(ACCENT_COLUMN as u16, row as u16)].symbol(), "┃");
        assert_eq!(
            buffer[(ACCENT_COLUMN as u16, row as u16)].fg,
            Color::Rgb(0xd2, 0xa6, 0xff)
        );
    }
    assert_eq!(
        buffer[(BULLET_COLUMN as u16, first_user_row as u16)].symbol(),
        "❯"
    );
    assert_eq!(
        buffer[(BULLET_COLUMN as u16, second_user_row as u16)].symbol(),
        "❯"
    );
    assert_eq!(
        buffer[(BULLET_COLUMN as u16, first_agent_row as u16)].symbol(),
        "●"
    );
    assert_eq!(
        buffer[(ACCENT_COLUMN as u16, first_agent_row as u16)].symbol(),
        " "
    );
    assert_eq!(
        buffer[(ACCENT_COLUMN as u16, second_agent_row as u16)].symbol(),
        " "
    );
    assert_eq!(
        buffer[(BULLET_COLUMN as u16, first_agent_row as u16)].fg,
        Color::Rgb(0x95, 0xe6, 0xcb)
    );
    assert!(
        second_user_row > second_agent_row,
        "a new user turn follows the answer it replies to"
    );
    assert_eq!(
        rendered_line(&renderer, second_user_row - 1).trim(),
        "",
        "a new user turn keeps one blank row above it"
    );
    assert_ne!(
        rendered_line(&renderer, second_user_row - 2).trim(),
        "",
        "only one blank row separates a user turn from what precedes it"
    );
}

#[test]
fn live_assistant_content_uses_the_user_body_column_at_normal_width() {
    assert_conversation_content_column(56, false);
}

#[test]
fn restored_assistant_content_uses_the_user_body_column_at_narrow_width() {
    assert_conversation_content_column(24, true);
}

fn assert_conversation_content_column(width: u16, restored: bool) {
    let terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    // Four columns of transcript margin plus the two-column shared row gutter.
    let content_width = usize::from(width - 6);
    let first_line = format!(
        "ASSISTANT_FIRST{}",
        "x".repeat(content_width - "ASSISTANT_FIRST".len())
    );
    let markdown = format!(
        "{first_line} ASSISTANT_WRAPPED\n\n```text\nASSISTANT_CODE\n```\n\n- ASSISTANT_LIST"
    );

    if restored {
        tui.replace_history(&[
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("USER_BODY".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("THINKING_BODY".into()),
                    MessagePart::ToolCall {
                        id: "call-1".into(),
                        name: "native::read".into(),
                        input: "{}".into(),
                    },
                    MessagePart::Text(markdown),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "TOOL_BODY".into(),
                    is_error: false,
                }],
            },
        ])
        .unwrap();
        // Finished restored history: reasoning and tool output, one key each.
        tui.handle(Event::Key(Key::CtrlT));
        tui.handle(Event::Key(Key::CtrlO));
    } else {
        tui.begin_submission("USER_BODY");
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
            "THINKING_BODY".into(),
        )));
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: "call-1".into(),
            name: "native::read".into(),
            input: "{}".into(),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "TOOL_BODY".into(),
            is_error: false,
        }));
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(markdown)));
        // Live turn: Ctrl+O expands tools while thinking stays streaming-expanded.
        tui.handle(Event::Key(Key::CtrlO));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains('❯'), "{text:?}");
    assert!(text.contains("USER_BODY"), "{text:?}");
    for content in [
        "ASSISTANT_FIRST",
        "ASSISTANT_WRAPPED",
        "THINKING_BODY",
        "• ASSISTANT_LIST",
        "read {}",
    ] {
        // Full-width transcript content shares a stable left margin.
        assert!(
            rendered_column(&renderer, content) <= 6,
            "{content} at col {}: {text:?}",
            rendered_column(&renderer, content)
        );
    }
    // A tool's output sits right of the call it belongs to, so a reader can
    // tell what the agent printed from what it ran without reading either.
    assert_eq!(
        rendered_column(&renderer, "TOOL_BODY"),
        rendered_column(&renderer, "read {}") + 2,
        "{text:?}"
    );
    // A fenced body sits inside its own panel rail, which owns the content column.
    assert!(
        rendered_column(&renderer, "ASSISTANT_CODE") <= 8,
        "ASSISTANT_CODE at col {}: {text:?}",
        rendered_column(&renderer, "ASSISTANT_CODE")
    );
    assert!(!text.contains("Assistant"), "{text:?}");
}

#[test]
fn thinking_streams_expanded_auto_collapses_on_finish_and_ctrl_t_re_expands() {
    let terminal = Terminal::new(TestBackend::new(64, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "**THOUGHTTOKEN**\n\n- inspect\n- verify".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let streaming = rendered_text(&renderer);
    assert_eq!(streaming.matches("Thinking").count(), 1, "{streaming:?}");
    assert!(!streaming.contains("Thought"), "{streaming:?}");
    assert!(streaming.contains("THOUGHTTOKEN"), "{streaming:?}");
    assert!(!streaming.contains("**"), "{streaming:?}");
    assert!(
        cell_for_text(&renderer, "THOUGHTTOKEN")
            .modifier
            .contains(Modifier::BOLD)
    );

    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));
    renderer.render(tui.view()).unwrap();
    let collapsed = rendered_text(&renderer);
    assert!(collapsed.contains("Thought"), "{collapsed:?}");
    assert!(!collapsed.contains("Thinking"), "{collapsed:?}");
    assert!(!collapsed.contains("THOUGHTTOKEN"), "{collapsed:?}");
    assert!(tui.view().collapse_thinking);

    tui.handle(Event::Key(Key::CtrlT));
    renderer.render(tui.view()).unwrap();
    let reexpanded = rendered_text(&renderer);
    assert!(reexpanded.contains("THOUGHTTOKEN"), "{reexpanded:?}");
    assert!(!reexpanded.contains("Thought"), "{reexpanded:?}");
    assert!(!tui.view().collapse_thinking);

    // Pin: a later finish path must not re-collapse user-expanded thinking.
    tui.set_running(true);
    tui.set_running(false);
    assert!(!tui.view().collapse_thinking);
    renderer.render(tui.view()).unwrap();
    let pinned = rendered_text(&renderer);
    assert!(pinned.contains("THOUGHTTOKEN"), "{pinned:?}");
}

#[test]
fn finished_tool_row_keeps_name_args_and_status_on_one_collapsed_line() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "src/lib.rs".into(),
        parsed: agens_core::ToolInput::Other {
            name: "native::read".into(),
            raw: "src/lib.rs".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: "secret-tool-body-sentinel".into(),
        is_error: false,
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("┌ read") || text.contains(" read "),
        "{text:?}"
    );
    assert!(text.contains("src/lib.rs"), "{text:?}");
    assert!(text.contains("Success"), "{text:?}");
    assert!(!text.contains("output collapsed"), "{text:?}");
    assert!(!text.contains("secret-tool-body-sentinel"), "{text:?}");
    assert_eq!(
        transcript_rows(&renderer)
            .iter()
            .filter(|row| row.contains("read src/lib.rs"))
            .count(),
        1,
        "{text:?}"
    );
    assert!(
        !text.contains("native::read · read-1"),
        "call_id must stay off the scan path: {text:?}"
    );
    assert!(
        !text.contains("┌ read-1"),
        "call_id must not be the primary result label: {text:?}"
    );

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(
        expanded.contains("secret-tool-body-sentinel"),
        "{expanded:?}"
    );
    assert!(expanded.contains("read"), "{expanded:?}");
    assert!(expanded.contains("src/lib.rs"), "{expanded:?}");
}

/// Raw JSON input for the shared live/restored collapse fixtures below.
const COLLAPSE_FIXTURE_INPUT: &str = r#"{"command":"cargo test --workspace","timeout_ms":600000}"#;
const COLLAPSE_FIXTURE_BODY: &str = "body-sentinel-line\nsecond body line\nthird body line";

/// Transcript rows a settled `native::bash` call leaves behind, live or restored.
fn collapse_fixture_rows(renderer: &RatatuiRenderer<TestBackend>) -> Vec<String> {
    transcript_rows(renderer)
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .collect()
}

fn assert_settled_call_is_one_hidden_row(rows: &[String], origin: &str) {
    let text = rows.join("\n");

    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("cargo test --workspace"))
            .count(),
        1,
        "{origin}: a settled call keeps exactly one row: {rows:?}"
    );
    assert!(
        text.contains("Success"),
        "{origin}: the row still names its outcome: {rows:?}"
    );
    assert!(
        !text.contains("timeout_ms"),
        "{origin}: raw input stays out of the settled transcript: {rows:?}"
    );
    assert!(
        !text.contains("body-sentinel-line"),
        "{origin}: the result body stays out of the settled transcript: {rows:?}"
    );
}

#[test]
fn live_tool_call_collapses_when_it_ends_and_hides_its_raw_input() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("run the tests");
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "bash-1".into(),
        name: "native::bash".into(),
        input: COLLAPSE_FIXTURE_INPUT.into(),
    });
    tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
        call_id: "bash-1".into(),
        name: "native::bash".into(),
        input: COLLAPSE_FIXTURE_INPUT.into(),
        parsed: agens_core::ToolInput::Bash {
            command: "cargo test --workspace".into(),
        },
    });

    renderer.render(tui.view()).unwrap();
    let running = collapse_fixture_rows(&renderer).join("\n");
    assert!(
        running.contains("cargo test --workspace"),
        "a running call names its work: {running:?}"
    );
    assert!(
        !running.contains("timeout_ms"),
        "a running call does not dump its raw input: {running:?}"
    );

    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "bash-1".into(),
        content: COLLAPSE_FIXTURE_BODY.into(),
        is_error: false,
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "bash-1".into(),
        duration: Some(Duration::from_millis(120)),
        result: ToolResultState::Success,
    });

    renderer.render(tui.view()).unwrap();
    assert_settled_call_is_one_hidden_row(&collapse_fixture_rows(&renderer), "live");

    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let audited = collapse_fixture_rows(&renderer).join("\n");
    assert!(
        audited.contains("timeout_ms"),
        "the raw input stays reachable through the audit mode: {audited:?}"
    );
    assert!(
        audited.contains("body-sentinel-line"),
        "the body stays reachable through the audit mode: {audited:?}"
    );
}

#[test]
fn settled_tool_call_without_a_recorded_mode_collapses_and_advances_from_collapsed() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("run the tests");
    start_execution(&mut tui, 1, "build");
    // Settling a parent call while a subagent transcript is selected records the
    // mode against that subagent, so the main transcript reaches this call with
    // nothing recorded for it.
    tui.select_transcript(TranscriptId::Subagent(1));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "bash-1".into(),
        name: "native::bash".into(),
        input: COLLAPSE_FIXTURE_INPUT.into(),
    });
    tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
        call_id: "bash-1".into(),
        name: "native::bash".into(),
        input: COLLAPSE_FIXTURE_INPUT.into(),
        parsed: agens_core::ToolInput::Bash {
            command: "cargo test --workspace".into(),
        },
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "bash-1".into(),
        content: COLLAPSE_FIXTURE_BODY.into(),
        is_error: false,
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "bash-1".into(),
        duration: Some(Duration::from_millis(120)),
        result: ToolResultState::Success,
    });
    tui.select_transcript(TranscriptId::Main);

    renderer.render(tui.view()).unwrap();
    assert_settled_call_is_one_hidden_row(&collapse_fixture_rows(&renderer), "unrecorded");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let advanced = collapse_fixture_rows(&renderer).join("\n");
    assert!(
        advanced.contains("body-sentinel-line"),
        "the first press advances out of collapsed instead of re-collapsing: {advanced:?}"
    );
    assert!(
        !advanced.contains("timeout_ms"),
        "the raw input waits for the audit mode: {advanced:?}"
    );
}

#[test]
fn restored_tool_call_collapses_exactly_like_the_live_path() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    // Mirrors the production resume path, which parses each restored call's
    // input before projecting it.
    let history = agens_tui::Conversation::from_messages_with_parser(
        &[
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("run the tests".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: "bash-1".into(),
                    name: "native::bash".into(),
                    input: COLLAPSE_FIXTURE_INPUT.into(),
                }],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "bash-1".into(),
                    content: COLLAPSE_FIXTURE_BODY.into(),
                    is_error: false,
                }],
            },
        ],
        // `ToolInput::parse` lives in `agens-permissions`, which this surface may
        // not depend on; this stands in for what it yields for the fixture input.
        |_, _| agens_core::ToolInput::Bash {
            command: "cargo test --workspace".into(),
        },
    )
    .unwrap();
    tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
        message: "Resumed session 1.".into(),
        presentation: TuiPresentation::new("provider", "model", "session #1"),
        history,
        draft: None,
        resume_error: None,
        file_candidates: Vec::new(),
        palette_entries: Vec::new(),
    });

    renderer.render(tui.view()).unwrap();
    assert_settled_call_is_one_hidden_row(&collapse_fixture_rows(&renderer), "restored");
}

#[test]
fn tool_call_updates_one_row_in_place_when_it_finishes() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "atlas-1".into(),
        name: "mcp::atlas_get_document".into(),
        input: r#"{"detail":"full","slug":"spec","workspace":"agens"}"#.into(),
        parsed: agens_core::ToolInput::Other {
            name: "mcp::atlas_get_document".into(),
            raw: r#"{"detail":"full","slug":"spec","workspace":"agens"}"#.into(),
        },
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    let running_rows = transcript_rows(&renderer);
    let running = running_rows
        .iter()
        .filter(|row| row.contains("atlas_get_document"))
        .collect::<Vec<_>>();
    assert_eq!(running.len(), 1, "{running_rows:?}");
    assert!(running[0].contains("Running"), "{running:?}");

    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "atlas-1".into(),
        output: "document body".into(),
        is_error: false,
    })
    .unwrap();
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "atlas-1".into(),
        duration: Some(Duration::from_millis(673)),
        result: ToolResultState::Success,
    });

    renderer.render(tui.view()).unwrap();
    let settled_rows = transcript_rows(&renderer);
    let settled = settled_rows
        .iter()
        .filter(|row| row.contains("atlas_get_document"))
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 1, "{settled_rows:?}");
    assert!(settled[0].contains("Success · 673ms"), "{settled:?}");
    assert!(settled[0].contains("1 lines · 13 B"), "{settled:?}");
    assert!(
        !settled_rows
            .iter()
            .any(|row| row.contains("output collapsed"))
    );
    assert!(
        !settled_rows
            .iter()
            .any(|row| row.trim_start().starts_with("└"))
    );
}

#[test]
fn unfinished_tool_row_becomes_failure_when_the_turn_is_cancelled() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "src/lib.rs".into(),
        parsed: agens_core::ToolInput::Read {
            path: "src/lib.rs".into(),
        },
    })
    .unwrap();
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Cancelled {
        message: "cancelled".into(),
        action: "retry".into(),
    });

    renderer.render(tui.view()).unwrap();
    let row = transcript_rows(&renderer)
        .into_iter()
        .find(|row| row.contains("Read src/lib.rs"))
        .expect("tool row");
    assert!(row.contains("Failure"), "{row:?}");
    assert!(!row.contains("Running"), "{row:?}");
}

#[test]
fn typed_tool_headers_render_per_kind_and_keep_raw_arguments_behind_expand() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(110, 44)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for (call_id, name, input, parsed) in [
        (
            "read-1",
            "native::read",
            "{\"path\":\"src/main.rs\"}",
            agens_core::ToolInput::Read {
                path: "src/main.rs".into(),
            },
        ),
        (
            "bash-1",
            "native::bash",
            "{\"command\":\"cargo test\"}",
            agens_core::ToolInput::Bash {
                command: "cargo test".into(),
            },
        ),
        (
            "grep-1",
            "native::grep",
            "{\"pattern\":\"needle\",\"path\":\"src\"}",
            agens_core::ToolInput::Grep {
                pattern: "needle".into(),
                path: Some("src".into()),
            },
        ),
        (
            "mcp-1",
            "mcp::foo__bar",
            "{\"path\":\"/etc/hosts-sentinel\",\"limit\":10}",
            agens_core::ToolInput::Other {
                name: "mcp::foo__bar".into(),
                raw: "{\"path\":\"/etc/hosts-sentinel\",\"limit\":10}".into(),
            },
        ),
    ] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: input.into(),
            parsed,
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: call_id.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    for header in [
        "Read src/main.rs",
        "$ cargo test",
        "Grep needle in src",
        "foo__bar {limit=10, path=/etc/hosts-sentinel}",
    ] {
        assert!(text.contains(header), "missing {header:?}: {text:?}");
    }
    assert!(
        !text.contains("\"path\":"),
        "no raw JSON on the scan path: {text:?}"
    );

    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(
        expanded.contains("/etc/hosts-sentinel"),
        "full raw arguments stay reachable when expanded: {expanded:?}"
    );
}

#[test]
fn consecutive_reads_render_as_one_group_row_that_settles_into_past_tense() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: path.into(),
            name: "native::read".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Read { path: path.into() },
        })
        .unwrap();
    }

    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let first_frame = rendered_text(&renderer);
    assert!(
        first_frame.contains("◈ Reading 3 files…"),
        "{first_frame:?}"
    );
    assert!(!first_frame.contains("Read src/a.rs"), "{first_frame:?}");
    assert_eq!(
        cell_for_text(&renderer, "◈").fg,
        Color::Rgb(0x73, 0xd0, 0xff),
        "a running group carries the active accent: {first_frame:?}"
    );

    let group_row = rendered_line(&renderer, rendered_row(&renderer, "◈ Reading 3 files…"));
    tui.tick(Duration::from_millis(200));
    renderer.render(tui.view()).unwrap();
    let second_frame = rendered_text(&renderer);
    assert_eq!(
        rendered_line(&renderer, rendered_row(&renderer, "◈ Reading 3 files…")),
        group_row,
        "a group row keeps one shape across ticks; state lives in its colour: {second_frame:?}"
    );

    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: path.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }
    renderer.render(tui.view()).unwrap();
    let settled = rendered_text(&renderer);
    assert!(settled.contains("Read 3 files"), "{settled:?}");
    assert!(!settled.contains("Reading 3 files"), "{settled:?}");
    assert!(settled.contains("Ctrl+O to expand"), "{settled:?}");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(!expanded.contains("Read 3 files"), "{expanded:?}");
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        assert!(
            expanded.contains(&format!("Read {path}")),
            "expanding the group restores the individual rows: {expanded:?}"
        );
    }
}

/// Columns owned by the shared transcript gutter: the accent bar sits one column
/// left of the bullet at `CHROME_GUTTER`, its content two columns further right.
const BULLET_COLUMN: usize = CHROME_GUTTER as usize;
const CONTENT_COLUMN: usize = BULLET_COLUMN + 2;
const ACCENT_COLUMN: usize = BULLET_COLUMN - 1;

/// Transcript content rows, excluding the top border and the bottom scroll label.
fn transcript_rows(renderer: &RatatuiRenderer<TestBackend>) -> Vec<String> {
    let last = usize::from(composer_top_row(renderer)).saturating_sub(1);
    (1..last).map(|row| rendered_line(renderer, row)).collect()
}

fn first_glyph_column(row: &str) -> Option<usize> {
    row.char_indices()
        .find(|(_, glyph)| !glyph.is_whitespace())
        .map(|(index, _)| row[..index].chars().count())
}

#[test]
fn collapsed_thinking_occupies_exactly_one_row_and_names_the_finished_thought() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "THOUGHTTOKEN body".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Thought"), "{text:?}");
    assert!(
        !text.contains("collapsed"),
        "the collapsed state is the row itself, not a suffix: {text:?}"
    );
    assert!(!text.contains("THOUGHTTOKEN"), "{text:?}");

    let thought_row = rendered_row(&renderer, "Thought");
    assert_eq!(
        rendered_line(&renderer, thought_row).trim(),
        "Thought",
        "no duration is tracked for reasoning, so the bare form renders"
    );
    let rows = transcript_rows(&renderer);
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("Thought") || row.contains("Thinking"))
            .count(),
        1,
        "collapsed thinking is exactly one row: {rows:?}"
    );
}

#[test]
fn every_transcript_row_sits_on_the_shared_gutter() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "check the manifests".into(),
    )));
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Read {
            path: "Cargo.toml".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: "ok".into(),
        is_error: false,
    })
    .unwrap();
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "one focused workspace".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(
        "one focused workspace".into(),
    ));

    renderer.render(tui.view()).unwrap();
    let rows = transcript_rows(&renderer);

    for row in &rows {
        let Some(column) = first_glyph_column(row) else {
            continue;
        };
        assert!(
            [ACCENT_COLUMN, BULLET_COLUMN, CONTENT_COLUMN].contains(&column),
            "row {row:?} starts at column {column}, outside the shared gutter"
        );
    }
    assert_eq!(
        rendered_column(&renderer, "└"),
        BULLET_COLUMN,
        "the result footer rail shares the bullet column"
    );
    assert_eq!(
        rendered_column(&renderer, "Read Cargo.toml"),
        CONTENT_COLUMN
    );
    assert_eq!(rendered_column(&renderer, "◆"), BULLET_COLUMN);
}

#[test]
fn collapsed_tool_rows_pack_while_prose_keeps_exactly_one_blank_row() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for (call_id, name, parsed) in [
        (
            "read-1",
            "native::read",
            agens_core::ToolInput::Read {
                path: "Cargo.toml".into(),
            },
        ),
        (
            "grep-1",
            "native::grep",
            agens_core::ToolInput::Grep {
                pattern: "needle".into(),
                path: None,
            },
        ),
    ] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: "{}".into(),
            parsed,
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: call_id.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "PROSE_SENTINEL".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(
        "PROSE_SENTINEL".into(),
    ));

    renderer.render(tui.view()).unwrap();
    let rows = transcript_rows(&renderer);

    let read_row = rendered_row(&renderer, "Read Cargo.toml");
    let grep_row = rendered_row(&renderer, "Grep needle");
    let prose_row = rendered_row(&renderer, "PROSE_SENTINEL");
    for row in read_row..grep_row {
        assert!(
            !rendered_line(&renderer, row).trim().is_empty(),
            "collapsed tool rows pack without a blank row: {rows:?}"
        );
    }
    assert!(
        rendered_line(&renderer, prose_row - 1).trim().is_empty(),
        "prose keeps one blank row above it: {rows:?}"
    );
    assert!(
        !rendered_line(&renderer, prose_row - 2).trim().is_empty(),
        "prose keeps exactly one blank row above it: {rows:?}"
    );
}

#[test]
fn transcript_bullets_carry_state_in_colour_and_group_headers_own_their_glyph() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 44)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "bash-1".into(),
        name: "native::bash".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Bash {
            command: "cargo check".into(),
        },
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    assert_eq!(
        cell_for_text(&renderer, "◆").fg,
        Color::Rgb(0x73, 0xd0, 0xff),
        "a running activity row carries the active accent"
    );

    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "bash-1".into(),
        output: "boom".into(),
        is_error: true,
    })
    .unwrap();
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        cell_for_text(&renderer, "◆").fg,
        Color::Rgb(0xf0, 0x71, 0x78),
        "a failed activity row carries the error colour"
    );

    for path in ["src/a.rs", "src/b.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: path.into(),
            name: "native::read".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Read { path: path.into() },
        })
        .unwrap();
    }
    for (path, is_error) in [("src/a.rs", false), ("src/b.rs", true)] {
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: path.into(),
            output: "ok".into(),
            is_error,
        })
        .unwrap();
    }
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Read 2 files"), "{text:?}");
    assert!(
        text.contains("· 1 failed"),
        "a folded group names its failures: {text:?}"
    );
    assert_eq!(rendered_column(&renderer, "◈"), BULLET_COLUMN);
    assert_eq!(
        cell_for_text(&renderer, "◈").fg,
        Color::Rgb(0xf0, 0x71, 0x78),
        "a group with a failure carries the error colour"
    );
}

#[test]
fn a_running_row_carries_an_accent_bar_left_of_its_bullet_without_shifting_content() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "bash-1".into(),
        name: "native::bash".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Bash {
            command: "cargo check".into(),
        },
    })
    .unwrap();

    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let first = rendered_text(&renderer);
    assert_eq!(
        rendered_column(&renderer, "┃"),
        ACCENT_COLUMN,
        "the accent bar owns one column left of the bullet: {first:?}"
    );
    assert_eq!(rendered_column(&renderer, "◆"), BULLET_COLUMN);
    assert_eq!(rendered_column(&renderer, "$ cargo check"), CONTENT_COLUMN);
    let bar_row = rendered_row(&renderer, "$ cargo check");
    let initial_bar_color =
        renderer.terminal().backend().buffer()[(ACCENT_COLUMN as u16, bar_row as u16)].fg;
    assert_eq!(
        initial_bar_color,
        Color::Rgb(0x60, 0xae, 0xd6),
        "the running row starts at its own wave phase: {first:?}"
    );

    let shape = rendered_line(&renderer, bar_row);
    tui.tick(Duration::from_millis(240));
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        rendered_line(&renderer, bar_row),
        shape,
        "the row keeps one shape across ticks; only the accent colour moves"
    );
    assert_ne!(
        renderer.terminal().backend().buffer()[(ACCENT_COLUMN as u16, bar_row as u16)].fg,
        initial_bar_color,
        "the running bar breathes with the shared tick clock"
    );
}

#[test]
fn finished_read_rows_drop_the_accent_bar_while_a_folded_group_dims_it() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Read {
            path: "Cargo.toml".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: "ok".into(),
        is_error: false,
    })
    .unwrap();
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("done".into()));

    renderer.render(tui.view()).unwrap();
    let settled = rendered_text(&renderer);
    let read_row = rendered_row(&renderer, "Read Cargo.toml");
    let read_line = rendered_line(&renderer, read_row);
    assert!(
        !settled.contains('❙'),
        "a plain finished read never gains a collapsed accent: {settled:?}"
    );
    assert_eq!(
        read_line.chars().nth(ACCENT_COLUMN),
        Some(' '),
        "the accent column stays blank: {read_line:?}"
    );
    assert_eq!(
        rendered_column(&renderer, "Read Cargo.toml"),
        CONTENT_COLUMN
    );

    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for path in ["src/a.rs", "src/b.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: path.into(),
            name: "native::read".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Read { path: path.into() },
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: path.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("done".into()));
    renderer.render(tui.view()).unwrap();
    let folded = rendered_text(&renderer);

    assert!(folded.contains("Read 2 files"), "{folded:?}");
    assert_eq!(
        rendered_column(&renderer, "❙"),
        ACCENT_COLUMN,
        "a collapsed groupable run keeps the thin bar in the accent column: {folded:?}"
    );
    let Color::Rgb(red, green, blue) = cell_for_text(&renderer, "❙").fg else {
        panic!("the accent bar is painted in RGB: {folded:?}");
    };
    assert!(
        red < 0xaa && green < 0xd9 && blue < 0x4c,
        "the collapsed bar is dimmed against the group's own colour: {red:x} {green:x} {blue:x}"
    );
}

#[test]
fn accented_rows_render_without_panicking_on_small_terminals() {
    for (width, height) in [(1_u16, 1_u16), (2, 3), (6, 5), (10, 8), (24, 12), (40, 20)] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize { width, height });
        tui.begin_submission("inspect");
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: "bash-1".into(),
            name: "native::bash".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Bash {
                command: "cargo check".into(),
            },
        })
        .unwrap();
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
            "thinking body".into(),
        )));
        tui.tick(Duration::from_millis(240));

        renderer.render(tui.view()).unwrap();
        assert_eq!(
            rendered_text(&renderer).chars().count(),
            usize::from(width) * usize::from(height),
            "{width}x{height}"
        );
    }
}

#[test]
fn no_header_row_and_the_working_indicator_lives_at_the_end_of_the_chat() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("do work");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "assistant body".into(),
    )));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(3_000),
        output_tokens: Some(400),
        total_tokens: Some(3_400),
        context_window: Some(200_000),
    }));
    tui.tick(Duration::from_secs(12));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(
        rendered_line(&renderer, 0)
            .chars()
            .all(|glyph| glyph == '─'),
        "the first row is transcript content chrome, not a header strip: {:?}",
        rendered_line(&renderer, 0)
    );
    assert!(text.contains("12s"), "{text:?}");
    assert!(text.contains("3.4k tok"), "{text:?}");

    let body_row = rendered_row(&renderer, "assistant body");
    let indicator_row = rendered_row(&renderer, "3.4k tok");
    let footer_row = rendered_row(&renderer, "model —");
    assert!(
        body_row < indicator_row && indicator_row < footer_row,
        "the working indicator sits inside the chat: {body_row} {indicator_row} {footer_row}"
    );

    tui.set_running(false);
    renderer.render(tui.view()).unwrap();
    let idle = rendered_text(&renderer);
    assert!(
        !idle.contains("3400 tok"),
        "the working indicator disappears when idle: {idle:?}"
    );
}

/// Supervising delegated work means comparing branches against each other. A
/// row that reports only a state and a second count cannot be compared: elapsed
/// time is meaningless without knowing what each branch runs on, and raw
/// seconds past a minute are arithmetic rather than a reading.
#[test]
fn three_running_subagents_report_which_is_slowest_and_what_each_runs_on() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 100,
        height: 50,
    });
    tui.begin_submission("delegate");

    for (id, agent, model) in [
        (9_u64, "explore", "gpt-5.6-sol"),
        (10, "plan", "claude-opus-5"),
        (11, "build", "gpt-5.6-mini"),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started_on(
                id,
                agent,
                "task",
                TuiExecutionState::ForegroundRunning,
                Some(model),
                Some("high"),
            ),
        );
    }
    tui.tick(Duration::from_secs(253));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for model in ["gpt-5.6-sol", "claude-opus-5", "gpt-5.6-mini"] {
        assert!(text.contains(model), "missing {model:?}: {text:?}");
    }
    assert!(
        text.contains("4m 13s"),
        "elapsed changes unit rather than growing: {text:?}"
    );
    assert!(!text.contains("253s"), "{text:?}");
    assert!(
        text.contains("Main · 3 running"),
        "the root answers how much is in flight before any branch is read: {text:?}"
    );
}

/// Cancelling is the one panel action with a consequence outside the screen, so
/// the hint and the key have to agree: advertised exactly where a press would
/// cancel something, absent where it would do nothing.
#[test]
fn the_tree_offers_cancel_only_over_a_running_branch_and_the_key_cancels_it() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 100,
        height: 50,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 9 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(9, "explore", "task", TuiExecutionState::ForegroundRunning),
    );

    renderer.render(tui.view()).unwrap();
    assert!(
        !rendered_text(&renderer).contains("x cancel"),
        "nothing is selected yet, so the key would act on nothing"
    );

    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Char('l')));
    assert_eq!(tui.view().active_transcript, TranscriptId::Subagent(9));

    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("x cancel"),
        "{:?}",
        rendered_text(&renderer)
    );

    assert_eq!(
        tui.handle(Event::Key(Key::Char('x'))),
        Action::CancelExecution(9)
    );

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::Cancelled { id: 9 },
    });
    renderer.render(tui.view()).unwrap();
    assert!(
        !rendered_text(&renderer).contains("x cancel"),
        "a cancelled branch cannot be cancelled again: {:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn subagent_tree_renders_below_the_composer_and_owns_the_navigation_hints() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 100,
        height: 50,
    });
    tui.begin_submission("delegate");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "assistant body".into(),
    )));
    for (id, agent) in [(9_u64, "explore"), (10, "plan")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
        );
    }
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_call(9, "read-1", "native::read", "secret-child-input"),
    );
    tui.tick(Duration::from_secs(13));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(text.contains("├─"), "{text:?}");
    assert!(text.contains("└─"), "{text:?}");
    assert!(text.contains("Explore #9"), "{text:?}");
    assert!(text.contains("Plan #10"), "{text:?}");
    assert!(
        text.contains("└─ ● Read files"),
        "the focused branch expands into its live child activity: {text:?}"
    );
    assert!(!text.contains("secret-child-input"), "{text:?}");

    assert_eq!(
        text.matches("Tab focus · Enter inspect · Ctrl+B background")
            .count(),
        1,
        "the navigation hints live with the tree only: {text:?}"
    );
    let body_row = rendered_row(&renderer, "assistant body");
    let tree_row = rendered_row(&renderer, "Tab focus");
    let footer_row = rendered_row(&renderer, "model —");
    assert!(
        body_row < footer_row && footer_row < tree_row,
        "the tree sits below the composer border that carries the metadata: {body_row} {footer_row} {tree_row}"
    );

    let branch_row = rendered_row(&renderer, "Explore #9");
    tui.handle(Event::MouseDown {
        column: 4,
        row: u16::try_from(branch_row).unwrap(),
    });
    assert_eq!(
        tui.view().active_transcript,
        TranscriptId::Subagent(9),
        "clicking a tree branch navigates to that subagent"
    );
}

/// Long single-line summaries are the transcript's only unwrappable rows, so a
/// narrow terminal must see them elided rather than sliced mid-word.
#[test]
fn narrow_terminals_elide_long_summaries_on_a_word_boundary_instead_of_slicing_them() {
    const LONG_TASK: &str = "Investiga este proyecto sin modificar archivos. Revisa la \
         estructura, tecnologias usadas, puntos de entrada, scripts disponibles.";

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(40, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 40,
        height: 30,
    });
    tui.begin_submission("delegate");
    for (id, agent, task) in [
        (9_u64, "explore", LONG_TASK),
        (10, "deep-research-explorer", "task"),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(id, agent, task, TuiExecutionState::ForegroundRunning),
        );
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(
        text.contains("Explore · Investiga este proyecto…"),
        "the card keeps a first-sentence title elided on a word boundary: {text:?}"
    );
    assert!(
        !text.contains("Revisa"),
        "the card never dumps the rest of the prompt: {text:?}"
    );
    assert!(
        text.contains("Deep-research-explorer #10…"),
        "a tree branch label is elided instead of sliced: {text:?}"
    );
    assert!(
        text.contains("Tab focus · Enter inspect…"),
        "the tree affordance row is elided instead of sliced: {text:?}"
    );
}

#[test]
fn local_info_renders_once_in_the_footer_without_a_conversation_row() {
    let terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.apply_submission_outcome(agens_tui::TuiSubmissionOutcome::LocalInfo(
        "local-info-sentinel".into(),
    ));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert_eq!(text.matches("local-info-sentinel").count(), 1, "{text:?}");
    assert!(tui.transcript().is_empty());
    assert!(tui.view().conversation.is_none());
}

#[test]
fn fenced_code_block_chrome_does_not_nest_body_gutter_on_header() {
    let terminal = Terminal::new(TestBackend::new(56, 16)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "Use:\n\n```bash\nagens chat \"hello\"\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("╭─ bash"), "{text:?}");
    assert!(text.contains("agens chat"), "{text:?}");
    assert!(text.contains('╰'), "{text:?}");
    // Header must not nest the body gutter into chrome.
    assert!(!text.contains("│ ╭"), "{text:?}");
    assert!(!text.contains("│╭"), "{text:?}");
    // Body lines keep a single gutter cell; rails join ╭─ / │ / ╰─.
    assert!(text.contains("│ agens chat"), "{text:?}");
    // Full-width rule continues past the language label.
    assert!(
        text.contains("╭─ bash ─") || text.matches('─').count() >= 8,
        "expected continuous header rule: {text:?}"
    );
}

#[test]
fn fenced_code_block_is_compact_and_has_no_trailing_empty_body_row() {
    let width = 48_u16;
    let terminal = Terminal::new(TestBackend::new(width, 14)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```bash\nshort\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let header_row = rendered_row(&renderer, "╭─ bash");
    let body_row = rendered_row(&renderer, "short");
    let footer_row = rendered_row(&renderer, "╰");
    assert_eq!(body_row, header_row + 1);
    assert_eq!(footer_row, body_row + 1);

    let header = rendered_line(&renderer, header_row);
    let body = rendered_line(&renderer, body_row);
    let footer = rendered_line(&renderer, footer_row);
    assert!(header.contains("╭─ bash ╮"), "{header:?}");
    assert!(body.contains("│ short │"), "{body:?}");
    assert!(footer.contains("╰───────╯"), "{footer:?}");

    let borders =
        [(&header, '╭', '╮'), (&body, '│', '│'), (&footer, '╰', '╯')].map(|(line, left, right)| {
            let left = line
                .chars()
                .position(|character| character == left)
                .expect("left code border");
            let right = line
                .chars()
                .enumerate()
                .filter_map(|(index, character)| (character == right).then_some(index))
                .last()
                .expect("right code border");
            (left, right, right - left + 1)
        });
    assert!(
        borders.iter().all(|border| *border == borders[0]),
        "borders={borders:?} header={header:?} body={body:?} footer={footer:?}"
    );
    assert!(borders[0].1 < usize::from(width - 1));

    let buffer = renderer.terminal().backend().buffer();
    let panel = Color::Rgb(0x1a, 0x1f, 0x29);
    let panel_cells = (0..buffer.area.width)
        .filter(|x| buffer[(*x, body_row as u16)].bg == panel)
        .count();
    assert_eq!(panel_cells, 9);
}

#[test]
fn fenced_javascript_uses_token_specific_foregrounds() {
    let terminal = Terminal::new(TestBackend::new(72, 18)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```js\nconst answer = \"value\"; // note\nlet count = 42;\n```\n".into(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    renderer.render(tui.view()).unwrap();

    let colors =
        ["const", "\"value\"", "// note", "42"].map(|token| cell_for_text(&renderer, token).fg);
    let unique = colors.iter().fold(Vec::new(), |mut unique, color| {
        if !unique.contains(color) {
            unique.push(*color);
        }
        unique
    });

    assert!(unique.len() >= 3, "foregrounds: {colors:?}");
    assert!(
        colors
            .iter()
            .any(|color| *color != Color::Rgb(0xaa, 0xd9, 0x4c)),
        "code block stayed uniformly success-green: {colors:?}"
    );
}

#[test]
fn unknown_fence_language_uses_neutral_panel_style() {
    let terminal = Terminal::new(TestBackend::new(48, 14)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```not-a-language\nmystery_token 42\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let cell = cell_for_text(&renderer, "mystery_token");
    assert_eq!(cell.fg, Color::Rgb(0xbf, 0xbd, 0xb6));
    assert_eq!(cell.bg, Color::Rgb(0x1a, 0x1f, 0x29));
}

#[test]
fn paragraph_to_fence_has_one_blank_transition_row() {
    let terminal = Terminal::new(TestBackend::new(48, 20)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "Lead paragraph.\n\n```bash\nshort\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let paragraph_row = rendered_row(&renderer, "Lead paragraph.");
    let fence_row = rendered_row(&renderer, "╭─ bash");
    assert_eq!(fence_row, paragraph_row + 2);
}

#[test]
fn renderer_renders_practical_markdown_semantics() {
    let terminal = Terminal::new(TestBackend::new(72, 60)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "# Result\n\n## Section Two\n\n#### Detail Four\n\nUse **STRONGTOKEN** and *EMPHASISTOKEN* with `INLINE_TOKEN`.\n\n```rust\nfn example() {}\n```\n\n- first item\n- second item\n- [x] checked item\n- [ ] unchecked item\n\n> quoted text\n\n| Name | State |\n| --- | --- |\n| alpha | ready |\n| beta | blocked |\n\n[LINKTOKEN](https://example.com/docs)"
            .into(),
    )));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for absent in [
        "ASSISTANT",
        "**",
        "*EMPHASISTOKEN*",
        "```",
        "`INLINE_TOKEN`",
    ] {
        assert!(!text.contains(absent), "found {absent:?} in {text:?}");
    }
    for expected in [
        "Result",
        "Section Two",
        "Detail Four",
        "STRONGTOKEN",
        "EMPHASISTOKEN",
        "INLINE_TOKEN",
        "rust",
        "fn example() {}",
        "first item",
        "quoted text",
        "LINKTOKEN",
        "https://example.com/docs",
        "│ Name  │ State   │",
        "│ alpha │ ready   │",
        "│ beta  │ blocked │",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }

    let table_separators = ["│ Name", "│ alpha", "│ beta"].map(|needle| {
        let row = rendered_row(&renderer, needle);
        rendered_line(&renderer, row)
            .char_indices()
            .filter_map(|(index, character)| (character == '│').then_some(index))
            .collect::<Vec<_>>()
    });
    assert_eq!(table_separators[0], table_separators[1]);
    assert_eq!(table_separators[1], table_separators[2]);

    assert!(
        cell_for_text(&renderer, "Result")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_for_text(&renderer, "STRONGTOKEN")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_for_text(&renderer, "EMPHASISTOKEN")
            .modifier
            .contains(Modifier::ITALIC)
    );
    let body = Color::Rgb(0xbf, 0xbd, 0xb6);
    let markdown_heading = Color::Rgb(0x95, 0xe6, 0xcb);
    let markdown_strong = Color::Rgb(0xd6, 0xd4, 0xcd);
    let markdown_code = Color::Rgb(0xff, 0x8f, 0x40);
    let navigation = Color::Rgb(0x95, 0xe6, 0xcb);
    let success = Color::Rgb(0xaa, 0xd9, 0x4c);

    let inline_code = cell_for_text(&renderer, "INLINE_TOKEN");
    assert_eq!(inline_code.fg, markdown_code);
    assert_eq!(
        inline_code.bg,
        Color::Rgb(0x1a, 0x1f, 0x29),
        "inline code combines a warm foreground with its panel"
    );
    let link = cell_for_text(&renderer, "LINKTOKEN");
    assert_eq!(link.fg, navigation);
    assert!(link.modifier.contains(Modifier::UNDERLINED));
    assert_eq!(cell_for_text(&renderer, "STRONGTOKEN").fg, markdown_strong);
    assert_eq!(cell_for_text(&renderer, "Result").fg, markdown_heading);
    assert!(
        cell_for_text(&renderer, "Result")
            .modifier
            .contains(Modifier::UNDERLINED),
        "H1 should underline for hierarchy"
    );
    assert_eq!(cell_for_text(&renderer, "EMPHASISTOKEN").fg, body);
    assert_eq!(cell_for_text(&renderer, "•").fg, navigation);
    assert_eq!(cell_for_text(&renderer, "[x]").fg, success);
    assert_eq!(cell_for_text(&renderer, "[ ]").fg, navigation);
    assert_eq!(cell_for_text(&renderer, "◇").fg, markdown_heading);
    assert_eq!(cell_for_text(&renderer, "▫").fg, navigation);
    assert!(
        cell_for_text(&renderer, "Section Two")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        !cell_for_text(&renderer, "Detail Four")
            .modifier
            .contains(Modifier::BOLD),
        "lower headings should step down in weight as well as colour and marker"
    );
    let quote_row = rendered_row(&renderer, "quoted text") as u16;
    let quote_rail_column = rendered_column(&renderer, "quoted text").saturating_sub(2) as u16;
    assert_eq!(
        renderer.terminal().backend().buffer()[(quote_rail_column, quote_row)].fg,
        markdown_heading,
        "quotes carry a distinct semantic rail"
    );
}

#[test]
fn markdown_lists_wrap_with_hanging_indent_and_stay_compact_between_blocks() {
    let terminal = Terminal::new(TestBackend::new(72, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "Before.\n\n- FIRST_ITEM alpha bravo charlie delta echo foxtrot golf hotel LONG_LIST_TAIL\n- SECOND_ITEM\n\nAfter.".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let before = rendered_row(&renderer, "Before.");
    let first = rendered_row(&renderer, "FIRST_ITEM");
    let tail = rendered_row(&renderer, "LONG_LIST_TAIL");
    let second = rendered_row(&renderer, "SECOND_ITEM");
    let after = rendered_row(&renderer, "After.");
    assert_eq!(first, before + 2);
    assert_eq!(
        rendered_column(&renderer, "LONG_LIST_TAIL"),
        rendered_column(&renderer, "FIRST_ITEM"),
        "wrapped list text keeps a hanging indent"
    );
    assert_eq!(second, tail + 1);
    assert_eq!(after, second + 2);
}

#[test]
fn wrapped_markdown_preserves_heading_quote_task_and_grapheme_structure() {
    let terminal = Terminal::new(TestBackend::new(48, 36)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "## HEADING_START alpha bravo charlie delta echo HEADING_TAIL\n\n> QUOTE_START alpha bravo charlie delta echo QUOTE_TAIL\n\n- [x] TASK_START alpha bravo charlie delta echo TASK_TAIL\n\nEmoji 👩‍💻 cafe\u{301} alpha bravo charlie delta EMOJI_TAIL"
            .into(),
    )));
    renderer.render(tui.view()).unwrap();

    for (start, tail) in [
        ("HEADING_START", "HEADING_TAIL"),
        ("QUOTE_START", "QUOTE_TAIL"),
        ("TASK_START", "TASK_TAIL"),
    ] {
        let continuation = rendered_line(&renderer, rendered_row(&renderer, tail));
        let first_text_column = continuation
            .chars()
            .position(char::is_alphanumeric)
            .expect("wrapped continuation text");
        assert_eq!(
            first_text_column,
            rendered_column(&renderer, start),
            "{tail} loses its structural hanging indent: {continuation:?}"
        );
    }
    let text = rendered_text(&renderer);
    assert!(text.contains("👩‍💻"), "ZWJ emoji was split: {text:?}");
    assert!(
        text.contains("cafe\u{301}"),
        "combining grapheme was split: {text:?}"
    );
}

#[test]
fn streamed_and_final_markdown_share_one_stable_rendering_path() {
    let terminal = Terminal::new(TestBackend::new(64, 20)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    let markdown = "## Stable heading\n\nA **stable-answer-token**.";

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(markdown.into())));
    renderer.render(tui.view()).unwrap();
    let live = rendered_text(&renderer);
    let live_row = rendered_row(&renderer, "stable-answer-token");

    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(markdown.into()));
    renderer.render(tui.view()).unwrap();
    let final_text = rendered_text(&renderer);

    assert_eq!(live.matches("stable-answer-token").count(), 1, "{live:?}");
    assert_eq!(
        final_text.matches("stable-answer-token").count(),
        1,
        "{final_text:?}"
    );
    // Idle header collapses after the turn ends (reclaims one row); markdown path stays stable.
    let final_row = rendered_row(&renderer, "stable-answer-token");
    assert!(
        final_row == live_row || final_row + 1 == live_row,
        "live_row={live_row} final_row={final_row}"
    );
    assert!(!live.contains("##"), "{live:?}");
    assert!(!final_text.contains("**"), "{final_text:?}");
}

#[test]
fn completed_turn_collapses_duplicated_live_stream_into_one_assistant_body() {
    let terminal = Terminal::new(TestBackend::new(72, 16)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    let once = "hola-unique-token";

    tui.begin_submission("request");
    // Simulate dual progress emission of the same completed body.
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(once.into())));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(once.into())));
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        rendered_text(&renderer).matches(once).count(),
        2,
        "precondition: live path still shows both deltas"
    );

    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(once.into()));
    renderer.render(tui.view()).unwrap();
    let final_text = rendered_text(&renderer);
    assert_eq!(
        final_text.matches(once).count(),
        1,
        "Completed must heal exact dual-progress duplication: {final_text:?}"
    );
}

#[test]
fn renderer_projects_conversation_losslessly_by_call_id() {
    let backend = TestBackend::new(120, 50);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("review the patch");
    for event in [
        ConversationEvent::ReasoningDelta("inspect every changed line".into()),
        ConversationEvent::MarkdownDelta("stale live markdown".into()),
        ConversationEvent::MarkdownFinal("final **markdown**".into()),
        ConversationEvent::ToolCall {
            call_id: "read-1".into(),
            name: "native::read".into(),
            input: "src/render.rs".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::read".into(),
                raw: "src/render.rs".into(),
            },
        },
        ConversationEvent::ToolCall {
            call_id: "write-2".into(),
            name: "native::write".into(),
            input: "src/render.rs".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::write".into(),
                raw: "src/render.rs".into(),
            },
        },
        ConversationEvent::ToolResult {
            call_id: "write-2".into(),
            output: "write result".into(),
            is_error: false,
        },
        ConversationEvent::ToolResult {
            call_id: "read-1".into(),
            output: "```text\nread result\n```".into(),
            is_error: false,
        },
        ConversationEvent::Diff {
            call_id: "edit-1".into(),
            lines: vec![DiffLine::new(8, DiffLineKind::Added, "new line")],
        },
        ConversationEvent::Error {
            message: "Request failed safely".into(),
            action: "Check credentials and retry.".into(),
        },
    ] {
        tui.apply_conversation_event(event).unwrap();
    }
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "read-1".into(),
        duration: Some(Duration::from_millis(12)),
        result: ToolResultState::Success,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(3),
        output_tokens: Some(5),
        total_tokens: Some(8),
        context_window: Some(128),
    }));

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for expected in [
        "final markdown",
        "inspect every changed line",
        "read",
        "src/render.rs",
        "read result",
        "write",
        "write result",
        "12ms",
        "new line",
        "6%",
        "Request failed safely",
        "Action: Check credentials and retry.",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("unavailable"), "{text:?}");
    assert!(!text.contains("context 128"), "{text:?}");
    assert!(!text.contains("stale live markdown"), "{text:?}");
    assert!(!text.contains("**"), "{text:?}");
    assert!(
        text.contains("```text"),
        "a tool result is terminal text, so a fence the tool printed stays literal: {text:?}"
    );
    assert!(!text.contains("native::read · read-1"), "{text:?}");
    assert!(!text.contains("native::write · write-2"), "{text:?}");
    assert_eq!(text.matches("Tools").count(), 1, "{text:?}");
    assert_eq!(text.matches("Error").count(), 1, "{text:?}");
    assert!(text.find("read src/render.rs").unwrap() < text.find("write src/render.rs").unwrap());
}

#[test]
fn lifecycle_metrics_render_in_footer_without_transcript_rows() {
    let backend = TestBackend::new(140, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        context_window: Some(8_192),
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: agens_core::TurnState::Completed,
        duration: Some(Duration::from_millis(25)),
    });

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(!text.contains("TURN"));
    assert!(!text.contains("USAGE"));
    assert!(text.contains("Completed"));
    assert!(text.contains("25ms"));
    assert!(text.contains("0%"), "{text:?}");
    assert!(!text.contains("context 8192"), "{text:?}");
    assert!(!text.contains("unavailable"), "{text:?}");
}

#[test]
fn renderer_recovers_collapsed_long_tool_output_in_a_bounded_viewport() {
    let backend = TestBackend::new(48, 12);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "large.log".into(),
        parsed: agens_core::ToolInput::Other {
            name: "native::read".into(),
            raw: "large.log".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: format!("short preview\n{}", "full-output-sentinel ".repeat(12)),
        is_error: false,
    })
    .unwrap();
    renderer.render(tui.view()).unwrap();
    let collapsed = rendered_text(&renderer);
    assert!(collapsed.contains("Success"), "{collapsed:?}");
    assert!(!collapsed.contains("output collapsed"), "{collapsed:?}");
    assert!(!collapsed.contains("full-output-sentinel"), "{collapsed:?}");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(expanded.contains("full-output-sentinel"), "{expanded:?}");
}

#[test]
fn renderer_recovers_complete_long_output_through_production_scroll_offsets() {
    let backend = TestBackend::new(48, 12);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 48,
        height: 12,
    });

    tui.begin_submission("request");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "large.log".into(),
        parsed: agens_core::ToolInput::Other {
            name: "native::read".into(),
            raw: "large.log".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: format!(
            "output-start-sentinel\n{}\noutput-end-sentinel",
            (0..40)
                .map(|line| format!("output-middle-{line:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        is_error: false,
    })
    .unwrap();

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("output-end-sentinel"));

    let mut traversal = String::new();
    for _ in 0..100 {
        tui.handle(Event::Key(Key::ScrollUp));
        renderer.render(tui.view()).unwrap();
        traversal.push_str(&rendered_text(&renderer));
    }
    for _ in 0..100 {
        tui.handle(Event::Key(Key::ScrollDown));
        renderer.render(tui.view()).unwrap();
        traversal.push_str(&rendered_text(&renderer));
    }
    assert!(traversal.contains("output-start-sentinel"));
    assert!(rendered_text(&renderer).contains("output-end-sentinel"));
}

#[test]
fn renderer_retains_completed_turns_while_streaming_and_scrolling_the_next_turn() {
    let backend = TestBackend::new(52, 16);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 52,
        height: 16,
    });

    tui.begin_submission("first-user-sentinel");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "first-reasoning-sentinel".into(),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "first-call".into(),
        name: "native::read".into(),
        input: "first-input".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "first-call".into(),
        content: "first-result-sentinel".into(),
        is_error: false,
    }));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(
        "first-answer-sentinel".into(),
    ));

    tui.begin_submission("second-user-sentinel");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "second-answer-sentinel".into(),
    )));

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("second-answer-sentinel"));

    let mut history = rendered_text(&renderer);
    for _ in 0..30 {
        tui.handle(Event::Key(Key::PageUp));
        renderer.render(tui.view()).unwrap();
        history.push_str(&rendered_text(&renderer));
    }
    for expected in [
        "first-user-sentinel",
        "first-reasoning-sentinel",
        "read",
        "first-input",
        "first-answer-sentinel",
    ] {
        assert!(history.contains(expected), "missing {expected:?}");
    }
    assert!(!history.contains("first-result-sentinel"), "{history:?}");
    assert!(!history.contains("first-call"), "{history:?}");

    // While the next turn is streaming, Ctrl+O targets tools (thinking is live).
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let mut expanded = rendered_text(&renderer);
    for _ in 0..30 {
        tui.handle(Event::Key(Key::PageDown));
        renderer.render(tui.view()).unwrap();
        expanded.push_str(&rendered_text(&renderer));
    }
    assert!(expanded.contains("Success"));
    assert!(expanded.contains("first-result-sentinel"));
}

#[test]
fn restored_history_scroll_stays_fixed_while_streaming_and_end_resumes_follow() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(52, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let mut messages = Vec::new();
    for turn in 0..12 {
        messages.push(Message {
            role: Role::User,
            parts: vec![MessagePart::Text(format!("restored-user-{turn:02}"))],
        });
        messages.push(Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(format!("restored-answer-{turn:02}"))],
        });
    }
    tui.replace_history(&messages).unwrap();
    tui.begin_submission("live-user-sentinel");
    tui.handle(Event::Resize {
        width: 52,
        height: 14,
    });
    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    let before = rendered_text(&renderer);
    assert!(before.contains("restored-user-10"), "{before:?}");
    assert!(before.contains("SCROLL"), "{before:?}");

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        (0..20)
            .map(|line| format!("streaming-line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    renderer.render(tui.view()).unwrap();
    let streamed = rendered_text(&renderer);
    assert!(streamed.contains("restored-user-10"), "{streamed:?}");
    assert!(!tui.following_bottom());

    // The top of a long transcript is the elision row, not its oldest turn:
    // scrolling there is exactly where the key that unfolds it has to be found.
    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    let top = rendered_text(&renderer);
    assert!(!top.contains("restored-user-00"), "{top:?}");
    assert!(top.contains("earlier turns · ^Y to show"), "{top:?}");
    assert!(!tui.following_bottom());

    tui.handle(Event::Key(Key::CtrlY));
    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("restored-user-00"));
    assert!(!tui.following_bottom());

    tui.handle(Event::Key(Key::End));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("streaming-line-19"));
    assert!(tui.following_bottom());
}

#[test]
fn restored_messages_render_every_turn_and_typed_part_in_persisted_order() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let message = |role, parts| Message { role, parts };
    let text = |value: &str| vec![MessagePart::Text(value.into())];
    let messages = vec![
        message(Role::User, text("first user")),
        message(
            Role::Assistant,
            vec![
                MessagePart::Reasoning("first reasoning".into()),
                MessagePart::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: "{}".into(),
                },
                MessagePart::Text("first answer".into()),
            ],
        ),
        message(
            Role::Tool,
            vec![MessagePart::ToolResult {
                tool_call_id: "c1".into(),
                content: "first result".into(),
                is_error: false,
            }],
        ),
        message(Role::System, text("persisted reminder")),
        message(Role::User, text("second user")),
        message(Role::Assistant, text("second answer")),
    ];
    tui.replace_history(&messages).unwrap();
    // Reasoning and tool output for restored finished history, one key each.
    tui.handle(Event::Key(Key::CtrlT));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    let order = "first user|first reasoning|read|first answer|first result|persisted reminder|second user|second answer";
    let mut offset = 0;
    for expected in order.split('|') {
        let position = text[offset..].find(expected).expect(expected);
        offset += position + expected.len();
    }
    assert!(!text.contains("read · c1"), "{text:?}");
    assert_eq!(text.matches('❯').count(), 2, "{text:?}");
    assert_eq!(text.matches("Thinking").count(), 1, "{text:?}");
    for label in ["USER", "ASSISTANT", "THINKING"] {
        assert!(!text.contains(label), "found {label:?} in {text:?}");
    }
}

#[test]
fn renderer_sanitizes_runtime_errors_and_preserves_the_action() {
    let backend = TestBackend::new(120, 40);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_conversation_event(ConversationEvent::Error {
        message: "api_key=key-sentinel; Authorization: header-sentinel; path: /path-sentinel; prompt: prompt-sentinel".into(),
        action: "Retry after updating credentials.".into(),
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    // Credential-shaped values are withheld, but only the value — never the whole message.
    for secret in ["key-sentinel", "header-sentinel"] {
        assert!(!text.contains(secret), "leaked {secret:?} in {text:?}");
    }
    assert!(
        !text.contains("[redacted]"),
        "whole message wiped: {text:?}"
    );
    assert!(
        text.contains("api_key=[redacted:"),
        "surrounding text around the key must survive: {text:?}"
    );
    assert!(
        text.contains("Authorization: [redacted:"),
        "surrounding text around the header must survive: {text:?}"
    );

    // `path:` and `prompt:` are not credential keys, and this sink is user-visible-only, so host
    // paths and prompt text are allowed to survive verbatim.
    for benign in ["path-sentinel", "prompt-sentinel"] {
        assert!(
            text.contains(benign),
            "benign text must survive on a user-visible-only sink: {benign:?} missing in {text:?}"
        );
    }

    assert!(
        text.contains("Action: Retry after updating credentials."),
        "{text:?}"
    );
}

#[test]
fn renderer_clips_a_generic_dialog_inside_the_viewport() {
    let backend = TestBackend::new(42, 14);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.show_dialog(
        "Details",
        "A bounded dialog body that remains inside the viewport.",
    );
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("─ Details"),
        "title sits in the border: {text:?}"
    );
    assert!(text.contains("bounded dialog body"), "{text:?}");
    assert!(text.contains("close"), "derived footer: {text:?}");
}

#[test]
fn diagnostics_dialog_wraps_a_long_line_instead_of_clipping_later_diagnostics() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(60, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 60,
        height: 12,
    });

    tui.add_diagnostic(
        "6 skills share a command name; /name runs the command, the skill tool still loads them: sdd-apply, sdd-verify",
    );
    tui.add_diagnostic("Command /shared has multiple definitions; applied source precedence.");
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("sdd-verify"),
        "tail of a wrapped line: {text:?}"
    );
    assert!(
        text.contains("precedence"),
        "a later diagnostic keeps its own rows: {text:?}"
    );
}

#[test]
fn renderer_clips_selection_help_options_current_and_disabled_states_after_resize() {
    let backend = TestBackend::new(28, 8);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.show_selection_dialog(DialogView::selection(
        "Choose a model",
        Some("Up/Down navigate, Enter selects, Esc cancels"),
        vec![
            DialogEntry::action("gpt-4.1 (current)", "model:gpt-4.1"),
            DialogEntry::disabled("future-model", "Unavailable"),
            DialogEntry::action("o3", "model:o3"),
        ],
    ));

    tui.handle(Event::Resize {
        width: 28,
        height: 8,
    });
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("─ Choose a model"), "{text:?}");
    assert!(text.contains("▌ gpt-4.1 (current)"), "{text:?}");
    // 28 columns leave 11 label cells once the badge is placed.
    assert!(text.contains("disabled future-mod"), "{text:?}");
    assert!(text.contains("navigate"), "derived footer: {text:?}");
    assert!(text.contains("select"), "derived footer: {text:?}");
    assert!(!text.contains("model:gpt-4.1"), "{text:?}");
}

#[test]
fn long_selection_dialog_scrolls_each_input_and_keeps_selection_visible_after_resize() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(30, 8)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 30,
        height: 8,
    });
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        Some("Navigate"),
        (0..20)
            .map(|index| DialogEntry::action(format!("Option {index:02}"), format!("pick:{index}")))
            .collect(),
    ));

    for _ in 0..8 {
        tui.handle(Event::Key(Key::Down));
    }
    renderer.render(tui.view()).unwrap();
    let arrows = rendered_text(&renderer);
    assert!(arrows.contains("▌ Option 08"), "{arrows:?}");
    assert!(!arrows.contains("Option 00"), "{arrows:?}");
    assert!(arrows.contains("█") || arrows.contains("│"), "{arrows:?}");

    tui.handle(Event::Key(Key::PageDown));
    renderer.render(tui.view()).unwrap();
    let page = rendered_text(&renderer);
    assert!(page.contains("▌ Option 11"), "{page:?}");

    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    let wheel = rendered_text(&renderer);
    assert!(wheel.contains("▌ Option 10"), "{wheel:?}");

    tui.handle(Event::Resize {
        width: 24,
        height: 5,
    });
    renderer.render(tui.view()).unwrap();
    let resized = rendered_text(&renderer);
    assert!(resized.contains("▌ Option 10"), "{resized:?}");

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('1')));
    for _ in 0..10 {
        tui.handle(Event::Key(Key::PageDown));
    }
    renderer.render(tui.view()).unwrap();
    let filtered = rendered_text(&renderer);
    assert!(filtered.contains("/ 1"), "{filtered:?}");
    assert!(filtered.contains("▌ Option 19"), "{filtered:?}");
    assert!(!filtered.contains("Option 08"), "{filtered:?}");
}

#[test]
fn session_dialog_renders_scope_hints_rows_details_and_distinct_empty_states() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(78, 16)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let current = DialogEntry::action_with_metadata(
        "#7 Alpha",
        "2 turns · 5m ago",
        "7 Alpha /work/alpha primary",
        "ID: 7 · Alpha\nTurns: 2 · Agent: primary\nUpdated: 100 (5m ago)",
        "session:7",
    );
    let other = DialogEntry::action_with_metadata(
        "#9 Beta",
        "4 turns · 1h ago",
        "9 Beta /work/beta reviewer",
        "ID: 9 · Beta\nTurns: 4 · Agent: reviewer\nUpdated: 90 (1h ago) · Root: /work/beta",
        "session:9",
    );
    tui.show_selection_dialog(DialogView::sessions_page(
        vec![current.clone()],
        SessionDialogRequest::initial(),
        Some(SessionDialogCursor::new(100, 7)),
    ));

    renderer.render(tui.view()).unwrap();
    let project = rendered_text(&renderer);
    assert!(
        project.contains("Resume session · Current project"),
        "{project:?}"
    );
    assert!(project.contains("ctrl+a all projects"), "{project:?}");
    assert!(project.contains("▌ #7 Alpha"), "{project:?}");
    assert!(project.contains("2 turns · 5m ago"), "{project:?}");
    assert!(project.contains("page 1 · more"), "{project:?}");
    assert!(
        !project.contains("Type to search |"),
        "session help prose lives in the footer now: {project:?}"
    );
    assert!(!project.contains("Agent: primary"), "{project:?}");
    assert!(!project.contains("Updated: 100"), "{project:?}");
    assert!(!project.contains("#9 Beta"), "{project:?}");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let details = rendered_text(&renderer);
    assert!(details.contains("Agent: primary"), "{details:?}");
    assert!(details.contains("Updated: 100 (5m ago)"), "{details:?}");

    let Action::LoadSessionPage(global_request) = tui.handle(Event::Key(Key::LineStart)) else {
        panic!("scope toggle should request a page");
    };
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        vec![current, other],
        global_request,
        None,
    )));
    renderer.render(tui.view()).unwrap();
    let global = rendered_text(&renderer);
    assert!(
        global.contains("Resume session · All projects"),
        "{global:?}"
    );
    assert!(global.contains("ctrl+a current project"), "{global:?}");
    assert!(global.contains("4 turns · 1h ago"), "{global:?}");
    assert!(!global.contains("root=/work/beta"), "{global:?}");
    assert!(!global.contains("Agent: primary"), "{global:?}");

    let mut search_request = None;
    tui.handle(Event::Key(Key::Char('/')));
    for character in "missing".chars() {
        let Action::LoadSessionPage(request) = tui.handle(Event::Key(Key::Char(character))) else {
            panic!("search should request a page");
        };
        search_request = Some(request);
    }
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        Vec::new(),
        search_request.unwrap(),
        None,
    )));
    renderer.render(tui.view()).unwrap();
    let search = rendered_text(&renderer);
    assert!(search.contains("No sessions match search."), "{search:?}");

    tui.show_selection_dialog(DialogView::sessions_page(
        Vec::new(),
        SessionDialogRequest::initial(),
        None,
    ));
    renderer.render(tui.view()).unwrap();
    let empty_project = rendered_text(&renderer);
    assert!(empty_project.contains("No resumable sessions in current project."));
    let Action::LoadSessionPage(empty_global_request) = tui.handle(Event::Key(Key::LineStart))
    else {
        panic!("scope toggle should request a page");
    };
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        Vec::new(),
        empty_global_request,
        None,
    )));
    renderer.render(tui.view()).unwrap();
    let empty_global = rendered_text(&renderer);
    assert!(empty_global.contains("No resumable sessions in any project."));

    tui.show_selection_dialog(DialogView::sessions_loading(SessionDialogRequest::initial()));
    renderer.render(tui.view()).unwrap();
    let loading = rendered_text(&renderer);
    assert!(loading.contains("Loading sessions…"), "{loading:?}");
    assert!(!loading.contains("No resumable sessions"), "{loading:?}");

    tui.show_selection_dialog(DialogView::sessions_error(
        SessionDialogRequest::initial(),
        "Saved sessions could not be loaded.",
    ));
    renderer.render(tui.view()).unwrap();
    let error = rendered_text(&renderer);
    assert!(
        error.contains("Saved sessions could not be loaded."),
        "{error:?}"
    );
    assert!(!error.contains("Loading sessions…"), "{error:?}");
}

#[test]
fn short_session_dialog_keeps_search_and_selected_row_visible_without_default_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(34, 7)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let entries = (0..12)
        .map(|id| {
            DialogEntry::action_with_metadata(
                format!("#{id} Session {id}"),
                "2 turns · now",
                format!("{id} Session {id} /work/alpha primary"),
                format!("Turns: 2 · Agent: primary\nID: {id} · Session {id}"),
                format!("session:{id}"),
            )
        })
        .collect::<Vec<_>>();
    tui.show_selection_dialog(DialogView::sessions_page(
        entries,
        SessionDialogRequest::initial(),
        None,
    ));
    tui.handle(Event::Resize {
        width: 34,
        height: 7,
    });
    tui.handle(Event::Key(Key::Char('/')));
    for _ in 0..8 {
        tui.handle(Event::Key(Key::Down));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(text.contains("/ "), "{text:?}");
    // 34 columns keep the right column and truncate the label instead.
    assert!(text.contains("▌ #8 Sessio"), "{text:?}");
    assert!(text.contains("2 turns · now"), "{text:?}");
    assert!(!text.contains("Agent: primary"), "{text:?}");
    assert!(!text.contains("#0 Session 0"), "{text:?}");
}

#[test]
fn subagent_inspect_dialog_renders_through_the_overlay_shell() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(70, 18)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for (id, agent) in [(9, "explore"), (10, "reviewer")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(
                id,
                agent,
                "inspect the overlay shell",
                TuiExecutionState::ForegroundRunning,
            ),
        );
    }

    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Char('g')));
    renderer.render(tui.view()).unwrap();
    let inspect = rendered_text(&renderer);

    assert!(inspect.contains("─ Subagents"), "{inspect:?}");
    assert!(inspect.contains("  Main"), "{inspect:?}");
    assert!(inspect.contains("▌ Explore #9"), "{inspect:?}");
    assert!(inspect.contains("  Reviewer #10"), "{inspect:?}");
    assert!(inspect.contains("navigate"), "derived footer: {inspect:?}");
    assert!(inspect.contains("select"), "derived footer: {inspect:?}");
    assert_eq!(
        cell_for_text(&renderer, "Explore #9").bg,
        Color::Rgb(0x1b, 0x33, 0x30),
        "the selection band spans the row"
    );
}

#[test]
fn read_only_dialog_renders_explicit_empty_and_clipped_selected_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(32, 7)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.show_selection_dialog(
        DialogView::read_only(
            "MCP servers",
            Some("Type to search | Enter details"),
            vec![DialogEntry::read_only(
                "remote-server-with-a-long-name  http  enabled/ready  32 tools",
                "remote http ready search fetch",
                "Source: global\nEndpoint: https://example.test/a/safe/path\nTools: search, fetch",
            )],
            "mcp",
        )
        .with_empty_message("No MCP servers configured."),
    );

    tui.handle(Event::Key(Key::Enter));
    renderer.render(tui.view()).unwrap();
    let details = rendered_text(&renderer);
    assert!(details.contains("Source: global"), "{details:?}");
    assert!(!details.contains("safe/path"), "{details:?}");
    assert!(details.contains("─ MCP servers"), "{details:?}");

    tui.show_selection_dialog(
        DialogView::read_only("MCP servers", Some("Search"), Vec::new(), "mcp")
            .with_empty_message("No MCP servers configured."),
    );
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("No MCP servers configured."));
}

#[test]
fn refreshable_dialog_footer_carries_the_refresh_shortcut() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.show_selection_dialog(DialogView::read_only(
        "MCP servers",
        None::<&str>,
        vec![DialogEntry::read_only(
            "remote  http  enabled/ready",
            "remote",
            "Source: global",
        )],
        "mcp",
    ));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("refresh"), "derived footer: {text:?}");

    tui.show_selection_dialog(DialogView::selection(
        "Choose a model",
        None::<&str>,
        vec![DialogEntry::action("gpt-4.1", "model:gpt-4.1")],
    ));
    renderer.render(tui.view()).unwrap();

    assert!(
        !rendered_text(&renderer).contains("refresh"),
        "only refreshable dialogs advertise the shortcut: {:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn renderer_draws_a_bounded_palette_overlay_without_reflowing_the_conversation() {
    let backend = TestBackend::new(34, 10);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.add_info("conversation sentinel");
    tui.set_palette_entries(vec![
        PaletteEntry::new(
            "connect",
            "Connect to ChatGPT",
            "[--device-auth]",
            PaletteEntryKind::BuiltIn,
        ),
        PaletteEntry::new(
            "review",
            "Review the patch",
            "[scope]",
            PaletteEntryKind::Command,
        ),
        PaletteEntry::new(
            "resume",
            "Resume a session",
            "<id>",
            PaletteEntryKind::BuiltIn,
        ),
    ]);

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("conversation sentinel"));
    let before = rendered_text(&renderer);

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('r')));
    renderer.render(tui.view()).unwrap();
    let palette = rendered_text(&renderer);

    assert!(palette.contains("commands"), "{palette:?}");
    assert!(palette.contains("/review"), "{palette:?}");
    assert!(palette.contains("/resume"), "{palette:?}");
    assert!(palette.contains("/review [scope]"), "{palette:?}");
    assert!(
        !palette.contains("Review the patch"),
        "34 columns degrade to a single column: {palette:?}"
    );
    assert!(
        !palette.contains("Resume a session"),
        "34 columns degrade to a single column: {palette:?}"
    );
    assert!(!palette.contains("[command]"), "{palette:?}");
    assert!(!palette.contains("[built-in]"), "{palette:?}");
    assert_eq!(
        cell_for_text(&renderer, "commands").fg,
        Color::Rgb(0x95, 0xe6, 0xcb)
    );
    assert_eq!(
        cell_for_text(&renderer, "─ commands").fg,
        Color::Rgb(0x6c, 0x73, 0x80)
    );
    assert!(palette.contains("▌ /review"), "{palette:?}");
    assert!(palette.contains("navigate"), "{palette:?}");
    assert!(palette.contains("close"), "{palette:?}");
    assert_eq!(
        cell_for_text(&renderer, "/review").bg,
        Color::Rgb(0x1b, 0x33, 0x30)
    );
    assert!(!palette.contains("/connect"), "{palette:?}");
    assert_ne!(before, palette);

    tui.handle(Event::Key(Key::Escape));
    renderer.render(tui.view()).unwrap();
    assert!(tui.transcript().is_empty());
    assert!(tui.view().status.is_none());
}

#[test]
fn renderer_draws_the_palette_description_column_right_aligned_when_wide() {
    let backend = TestBackend::new(100, 20);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.set_palette_entries(vec![
        PaletteEntry::new(
            "review",
            "Review the patch",
            "[scope]",
            PaletteEntryKind::Command,
        ),
        PaletteEntry::new(
            "resume",
            "Resume a session",
            "<id>",
            PaletteEntryKind::BuiltIn,
        ),
    ]);

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('r')));
    renderer.render(tui.view()).unwrap();
    let palette = rendered_text(&renderer);

    assert!(palette.contains("▌ /review [scope]"), "{palette:?}");
    assert!(palette.contains("Review the patch"), "{palette:?}");
    assert!(palette.contains("Resume a session"), "{palette:?}");
    assert_eq!(
        rendered_column(&renderer, "Review the patch"),
        rendered_column(&renderer, "Resume a session"),
        "equal-width descriptions share the right-aligned column"
    );
    assert_eq!(
        cell_for_text(&renderer, "Review the patch").fg,
        Color::Rgb(0xbf, 0xbd, 0xb6),
        "the selected row lifts its metadata out of the muted grey"
    );
    assert_eq!(
        cell_for_text(&renderer, "Resume a session").fg,
        Color::Rgb(0x5c, 0x67, 0x73),
        "unselected rows keep the muted metadata column"
    );
    assert_eq!(
        cell_for_text(&renderer, "Review the patch").bg,
        Color::Rgb(0x1b, 0x33, 0x30),
        "the selection band spans the full row"
    );
}

#[test]
fn renderer_shows_complete_rich_turn_details_without_truncation() {
    let backend = TestBackend::new(120, 40);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("review the patch");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "inspect every changed line".into(),
    )));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "live markdown stays visible".into(),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "native::read".into(),
        input: "src/render.rs".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-1".into(),
        content: "first line\nsecond line".into(),
        is_error: false,
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "src/render.rs".into(),
        parsed: agens_core::ToolInput::Read {
            path: "src/render.rs".into(),
        },
    });
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "read-1".into(),
        duration: Some(Duration::from_millis(12)),
        result: ToolResultState::Success,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Diff {
        call_id: "read-1".into(),
        lines: vec![
            DiffLine::new(7, DiffLineKind::Removed, "old line"),
            DiffLine::new(8, DiffLineKind::Added, "new line"),
        ],
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(3),
        output_tokens: Some(5),
        total_tokens: Some(8),
        context_window: Some(128),
    }));

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    for expected in [
        "inspect every changed line",
        "live markdown stays visible",
        "Read",
        "first line",
        "second line",
        "12ms",
        "old line",
        "new line",
        "6%",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("context 128"), "{text:?}");
}

#[test]
fn renderer_keeps_metrics_and_errors_readable_in_a_narrow_viewport() {
    let backend = TestBackend::new(42, 14);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.finish_submission(Err(
        "provider: request failed; retry after checking credentials".into(),
    ));
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: agens_core::TurnState::Failed,
        duration: Some(Duration::from_secs(2)),
    });

    renderer.render(tui.view()).unwrap();
    let text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        text.contains("provider: request failed"),
        "error is missing: {text:?}"
    );
    assert!(
        text.contains("Action:"),
        "error action is missing: {text:?}"
    );
    assert!(text.contains("2s"), "turn duration is missing: {text:?}");
}

#[test]
fn renderer_scrolls_multiline_unicode_composer_and_keeps_cursor_visible() {
    let terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    for character in "first\né🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();

    let cursor = renderer.terminal().backend().cursor_position();
    // TOP-only composer border: text starts at x=0 of the composer band.
    assert!(
        cursor.x < 30 && cursor.y < 10,
        "cursor must remain inside the terminal: {cursor:?}"
    );
    assert!(
        rendered_text(&renderer).contains("é🙂"),
        "{:?}",
        rendered_text(&renderer)
    );

    let terminal = Terminal::new(TestBackend::new(5, 8)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    for character in "ab🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();
    let cursor = renderer.terminal().backend().cursor_position();
    assert!(
        cursor.x < 5,
        "cursor must remain inside the composer: {cursor:?}"
    );
}

#[test]
fn physical_cursor_follows_main_composer_focus_and_overlay_ownership() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Key(Key::Char('x')));

    renderer.render(tui.view()).unwrap();
    assert!(renderer.terminal().backend().cursor_visible());

    tui.handle(Event::Key(Key::PageUp));
    for _ in 0..3 {
        renderer.render(tui.view()).unwrap();
        assert!(!renderer.terminal().backend().cursor_visible());
    }
    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());

    tui.handle(Event::Key(Key::Char('i')));
    renderer.render(tui.view()).unwrap();
    assert!(renderer.terminal().backend().cursor_visible());

    tui.show_selection_dialog(DialogView::selection(
        "Permission required",
        None::<String>,
        vec![DialogEntry::action("Allow once", "permission:1:allow-once")],
    ));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
    tui.handle(Event::Key(Key::Escape));

    tui.set_palette_entries(vec![PaletteEntry::new(
        "help",
        "Help",
        "",
        PaletteEntryKind::BuiltIn,
    )]);
    tui.handle(Event::Key(Key::LineStart));
    tui.handle(Event::Key(Key::DeleteToLineEnd));
    tui.handle(Event::Key(Key::Char('/')));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
    tui.handle(Event::Key(Key::Escape));

    tui.set_running(true);
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
    tui.set_running(false);

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(
            7,
            "explore",
            "inspect",
            agens_tui::TuiExecutionState::ForegroundRunning,
        ),
    );
    tui.select_transcript(TranscriptId::Subagent(7));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
}

#[test]
fn first_control_c_renders_a_confirmation_notice_instead_of_exiting() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    assert_eq!(
        tui.handle(Event::Key(Key::CtrlC)),
        agens_tui::Action::Render
    );
    renderer.render(tui.view()).unwrap();

    assert!(rendered_text(&renderer).contains("Press Ctrl+C again to exit"));
}

#[test]
fn session_loading_uses_exact_local_state_without_running_the_composer() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    assert!(tui.begin_session_load());

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("Loading session…"), "{rendered:?}");
    assert!(!rendered.contains("Working"), "{rendered:?}");
    assert!(!rendered.contains(" running "), "{rendered:?}");
    assert!(!tui.view().running);
}

#[test]
fn u15_c1b_renderer_shows_selected_and_all_active_recent_executions() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.set_agent_catalog(["reviewer", "writer", "tester", "triage"]);
    tui.select_agent("reviewer");
    for (agent, event) in [
        ("reviewer", TuiExecutionEvent::ForegroundStarted { id: 1 }),
        ("writer", TuiExecutionEvent::BackgroundStarted { id: 2 }),
        ("tester", TuiExecutionEvent::ForegroundStarted { id: 3 }),
        ("triage", TuiExecutionEvent::ForegroundStarted { id: 4 }),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
    }
    for (agent, event) in [
        ("tester", TuiExecutionEvent::Completed { id: 3 }),
        ("triage", TuiExecutionEvent::Failed { id: 4 }),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Main"), "{text:?}");
    assert!(text.contains("Reviewer #1"), "{text:?}");
    assert!(text.contains("Writer #2"), "{text:?}");
    assert!(text.contains("Triage #4"), "{text:?}");
    assert!(!text.contains("Tester #3"), "{text:?}");
    assert!(!text.contains("fg 1"), "old aggregate leaked: {text:?}");
}

#[test]
fn p1a1_renderer_collapses_live_tool_uses_and_expands_ordered_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 9 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(
            9,
            "reviewer",
            "review the rendering",
            TuiExecutionState::ForegroundRunning,
        ),
    );
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_call(9, "read", "native::read", "bounded input"),
    );
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_result(9, "read", "read result", false),
    );
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_call(9, "grep", "native::grep", "bounded pattern"),
    );

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(
        main.contains("● Reviewer · review the rendering")
            && main.contains("Running · foreground")
            && main.contains("Read files")
            && main.contains("Search code"),
        "{main:?}"
    );
    assert!(!main.contains("native::read"), "{main:?}");
    assert!(!main.contains("read result"), "{main:?}");
}

#[test]
fn p1a2_renderer_renders_terminal_status_final_result_and_ordered_expanded_tools() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 10 },
    });
    for event in [
        TuiSubagentEvent::started(
            10,
            "reviewer",
            "review terminal output",
            TuiExecutionState::ForegroundRunning,
        ),
        TuiSubagentEvent::tool_call(10, "first", "native::read", "first input"),
        TuiSubagentEvent::tool_result(10, "first", "first result", false),
        TuiSubagentEvent::tool_call(10, "second", "native::grep", "second input"),
    ] {
        apply_subagent(&mut tui, event);
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 10 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(10, SubagentStatus::Success, "terminal result"),
    );

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(
        main.contains("● Reviewer · review terminal output") && main.contains("Success · recent"),
        "{main:?}"
    );
    for child_row in ["terminal result", "read", "native::grep", "first result"] {
        assert!(
            !main.contains(child_row),
            "duplicated {child_row:?}: {main:?}"
        );
    }

    tui.select_transcript(TranscriptId::Subagent(10));
    renderer.render(tui.view()).unwrap();
    let child = rendered_text(&renderer);
    let pending_row = rendered_line(&renderer, rendered_row(&renderer, "second input"));
    assert!(pending_row.contains("Failure"), "{child:?}");
    assert!(!pending_row.contains("Running"), "{child:?}");
}

#[test]
fn subagent_cards_are_compact_statusful_and_never_render_raw_task_payloads() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.tick(Duration::from_secs(5));
    tui.begin_submission("delegate");
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "parent-task".into(),
        name: "task".into(),
        input: "raw-parent-task-input-secret".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "parent-task".into(),
        content: "raw-parent-task-output-secret".into(),
        is_error: false,
    }));
    for (id, name, input, output) in [
        (
            "task-control",
            "task_control",
            "raw-task-control-input-secret",
            "raw-task-control-output-secret",
        ),
        (
            "task-message",
            "native::task_message",
            "raw-task-message-input-secret",
            "raw-task-message-output-secret",
        ),
    ] {
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: id.into(),
            name: name.into(),
            input: input.into(),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: id.into(),
            content: output.into(),
            is_error: false,
        }));
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 9 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(
            9,
            "explore",
            "inspect the navigation",
            TuiExecutionState::ForegroundRunning,
        ),
    );

    renderer.render(tui.view()).unwrap();
    let initializing = rendered_text(&renderer);
    assert!(
        initializing.contains("● Explore · inspect the navigation"),
        "{initializing:?}"
    );
    assert!(initializing.contains("Initializing"), "{initializing:?}");
    assert!(!initializing.contains("raw-parent-task-input-secret"));
    assert!(!initializing.contains("raw-parent-task-output-secret"));
    assert!(!initializing.contains("raw-task-control-input-secret"));
    assert!(!initializing.contains("raw-task-control-output-secret"));
    assert!(!initializing.contains("raw-task-message-input-secret"));
    assert!(!initializing.contains("raw-task-message-output-secret"));

    tui.tick(Duration::from_secs(8));
    for (call_id, name, input) in [
        ("read", "native::read", "api_key=not-rendered"),
        ("grep", "native::grep", "token=not-rendered"),
        ("list", "native::list", "password=not-rendered"),
        ("unknown", "plugin::opaque", "secret=not-rendered"),
    ] {
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::tool_call(9, call_id, name, input),
        );
    }
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_result(9, "read", "child-output-secret", false),
    );

    renderer.render(tui.view()).unwrap();
    let running = rendered_text(&renderer);
    assert!(running.contains("Running"), "{running:?}");
    assert!(running.contains("3s"), "{running:?}");
    assert!(running.contains("Read files"), "{running:?}");
    assert!(running.contains("Search code"), "{running:?}");
    assert!(running.contains("List files"), "{running:?}");
    assert!(running.contains("+1 more activity"), "{running:?}");
    let title_row = rendered_row(&renderer, "● Explore · inspect");
    let last_card_row = rendered_row(&renderer, "+1 more activity");
    assert!(last_card_row.saturating_sub(title_row) < 7, "{running:?}");
    // Navigation affordances belong to the tree under the composer, not the card.
    let affordance_row = rendered_row(&renderer, "Ctrl+B background");
    assert!(affordance_row > last_card_row + 7, "{running:?}");
    for secret in [
        "api_key=not-rendered",
        "token=not-rendered",
        "password=not-rendered",
        "secret=not-rendered",
        "child-output-secret",
        "plugin::opaque",
    ] {
        assert!(!running.contains(secret), "leaked {secret:?}: {running:?}");
    }
}

#[test]
fn resumed_task_tool_rows_remain_stored_but_are_not_rendered_in_main() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.replace_history(&[
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("delegate restored".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "restored-task".into(),
                name: "native::task".into(),
                input: "restored-raw-input-sentinel".into(),
            }],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "restored-task".into(),
                content: "restored-raw-result-sentinel".into(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("restored parent summary".into())],
        },
    ])
    .unwrap();

    let stored = &tui.view().completed_conversations[0].tool_batches[0].calls[0];
    assert_eq!(stored.input, "restored-raw-input-sentinel");
    assert_eq!(
        stored.result.as_ref().unwrap().output,
        "restored-raw-result-sentinel"
    );
    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("restored parent summary"), "{rendered:?}");
    assert!(!rendered.contains("restored-raw-input-sentinel"));
    assert!(!rendered.contains("restored-raw-result-sentinel"));
    assert!(!rendered.contains("native::task"));
}

#[test]
fn subagent_terminal_status_and_elapsed_are_frozen_and_low_dimensions_are_safe() {
    let mut tui = Tui::new(FakeEngine);
    tui.tick(Duration::from_secs(5));
    for (id, agent) in [(9, "explore"), (10, "reviewer")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(
                id,
                agent,
                "bounded summary",
                TuiExecutionState::ForegroundRunning,
            ),
        );
    }
    tui.tick(Duration::from_secs(10));
    for (id, agent, event, status) in [
        (
            9,
            "explore",
            TuiExecutionEvent::Completed { id: 9 },
            SubagentStatus::Success,
        ),
        (
            10,
            "reviewer",
            TuiExecutionEvent::Failed { id: 10 },
            SubagentStatus::Failure,
        ),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
        apply_subagent(&mut tui, TuiSubagentEvent::terminal(id, status, "done"));
    }
    tui.tick(Duration::from_secs(20));
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::text(9, "post-terminal-sentinel"),
    );

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("Success · recent · 5s"), "{rendered:?}");
    assert!(rendered.contains("Failure · recent · 5s"), "{rendered:?}");
    assert!(!rendered.contains("post-terminal-sentinel"));

    for height in 1..=8 {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(20, height)).unwrap());
        renderer.render(tui.view()).unwrap();
        assert_eq!(
            rendered_text(&renderer).chars().count(),
            20 * usize::from(height)
        );
    }

    tui.set_palette_entries(vec![
        PaletteEntry::new(
            "review",
            "Review the patch",
            "[scope]",
            PaletteEntryKind::Command,
        ),
        PaletteEntry::new(
            "resume",
            "Resume a session",
            "<id>",
            PaletteEntryKind::BuiltIn,
        ),
    ]);
    tui.handle(Event::Key(Key::Char('/')));
    let low_dimensions = |tui: &Tui<FakeEngine>, label: &str| {
        for (width, height) in [(1, 1), (2, 3), (8, 4), (34, 10)] {
            let mut renderer =
                RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
            renderer.render(tui.view()).unwrap();
            assert_eq!(
                rendered_text(&renderer).chars().count(),
                usize::from(width) * usize::from(height),
                "{label} at {width}x{height}"
            );
        }
    };
    low_dimensions(&tui, "palette");

    tui.handle(Event::Key(Key::Escape));
    tui.show_selection_dialog(DialogView::selection(
        "Choose a model",
        Some("Up/Down navigate, Enter selects, Esc cancels"),
        (0..12)
            .map(|index| DialogEntry::action(format!("Option {index:02}"), format!("pick:{index}")))
            .collect(),
    ));
    low_dimensions(&tui, "selection dialog");

    tui.handle(Event::Key(Key::CtrlO));
    low_dimensions(&tui, "selection dialog with details");

    tui.show_selection_dialog(DialogView::sessions_page(
        vec![DialogEntry::action_with_metadata(
            "#7 Alpha",
            "2 turns · 5m ago",
            "7 alpha",
            "ID: 7 · Alpha\nTurns: 2",
            "session:7",
        )],
        SessionDialogRequest::initial(),
        Some(SessionDialogCursor::new(100, 7)),
    ));
    low_dimensions(&tui, "session dialog");

    tui.show_selection_dialog(DialogView::sessions_loading(SessionDialogRequest::initial()));
    low_dimensions(&tui, "loading session dialog");

    tui.show_selection_dialog(
        DialogView::selection(
            "Permission required",
            Some("native::read\n/work/alpha"),
            vec![
                DialogEntry::action("Allow once", "permission:1:allow-once"),
                DialogEntry::action("Deny once", "permission:1:deny-once"),
            ],
        )
        .as_confirm(),
    );
    low_dimensions(&tui, "confirm dialog");
}

#[test]
fn execution_strip_shows_main_and_at_most_three_prioritized_children() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for (id, agent, event) in [
        (1, "old", TuiExecutionEvent::ForegroundStarted { id: 1 }),
        (2, "recent", TuiExecutionEvent::ForegroundStarted { id: 2 }),
        (
            3,
            "background",
            TuiExecutionEvent::BackgroundStarted { id: 3 },
        ),
        (4, "active", TuiExecutionEvent::ForegroundStarted { id: 4 }),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(
                id,
                agent,
                "task",
                match event {
                    TuiExecutionEvent::BackgroundStarted { .. } => {
                        TuiExecutionState::BackgroundRunning
                    }
                    _ => TuiExecutionState::ForegroundRunning,
                },
            ),
        );
        tui.tick(Duration::from_secs(id));
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "old".into(),
        event: TuiExecutionEvent::Completed { id: 1 },
    });

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("Main"), "{rendered:?}");
    assert!(rendered.contains("Active #4"), "{rendered:?}");
    assert!(rendered.contains("Background #3"), "{rendered:?}");
    assert!(rendered.contains("Recent #2"), "{rendered:?}");
    assert!(!rendered.contains("Old #1"), "{rendered:?}");
}

#[test]
fn p1c2_renderer_shows_restored_tool_count_without_fabricating_tool_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::RestoredCompletedSubagent {
        id: 42,
        agent: "reviewer".into(),
        task_summary: "review the durable result".into(),
        final_result: "approved".into(),
        tool_uses: 3,
    });

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(main.contains("● Reviewer · review the durable result"));
    assert!(main.contains("Success · recent"));
    assert!(main.contains("+3 more activities"));
    assert!(!main.contains("Final result: approved"));
}

struct FixedPtyHarness {
    width: u16,
    height: u16,
    renderer: RatatuiRenderer<TestBackend>,
}

impl FixedPtyHarness {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            renderer: Self::renderer(width, height),
        }
    }

    fn resize(&mut self, tui: &mut Tui<FakeEngine>, width: u16, height: u16) {
        tui.handle(Event::Resize { width, height });
        self.width = width;
        self.height = height;
        self.renderer = Self::renderer(width, height);
    }

    fn render(&mut self, tui: &Tui<FakeEngine>) -> String {
        self.renderer.render(tui.view()).unwrap();
        rendered_text(&self.renderer)
    }

    fn cursor(&self) -> ratatui::layout::Position {
        self.renderer.terminal().backend().cursor_position()
    }

    fn renderer(width: u16, height: u16) -> RatatuiRenderer<TestBackend> {
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap())
    }
}

#[test]
fn structural_pty_resize_scroll_stream_and_dialog_contract() {
    let mut harness = FixedPtyHarness::new(52, 14);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("streaming request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        (0..24)
            .map(|line| format!("before-resize-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    harness.render(&tui);

    tui.handle(Event::Key(Key::ScrollUp));
    let scrolled = harness.render(&tui);
    assert!(
        scrolled.contains("before-resize-"),
        "scroll must keep prior stream rows visible: {scrolled:?}"
    );
    assert!(!tui.following_bottom());

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "streamed-after-scroll-sentinel".into(),
    )));
    let streamed = harness.render(&tui);
    assert!(
        streamed.contains("before-resize-"),
        "stream while scrolled must preserve anchor rows: {streamed:?}"
    );
    assert!(!streamed.contains("streamed-after-scroll-sentinel") || !tui.following_bottom());
    assert!(!tui.following_bottom());

    tui.handle(Event::Key(Key::End));
    let refollowed = harness.render(&tui);
    assert!(refollowed.contains("streamed-after-scroll-sentinel"));
    assert!(tui.following_bottom());

    tui.show_selection_dialog(DialogView::selection(
        "Recover interrupted attempt",
        Some("This may invalidate an attempt still running in another process."),
        vec![DialogEntry::action("Recover", "recover:7")],
    ));
    let dialog = harness.render(&tui);
    assert!(dialog.contains("Recover interrupted attempt"), "{dialog:?}");
    assert!(dialog.contains("still running"), "{dialog:?}");
    assert!(!dialog.contains("streaming request"), "{dialog:?}");

    tui.handle(Event::Key(Key::Escape));
    for character in "composer\né🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    harness.resize(&mut tui, 24, 8);
    let resized = harness.render(&tui);
    assert!(
        resized.contains("composer") || resized.contains("é🙂"),
        "{resized:?}"
    );
    let cursor = harness.cursor();
    assert!(cursor.x < 24 && cursor.y < 8, "{cursor:?}");

    for height in 1..=12 {
        harness.resize(&mut tui, 40, height);
        let surface = harness.render(&tui);
        assert_eq!(surface.chars().count(), 40 * usize::from(height));
        assert!(!surface.contains("agens safe"), "height {height}");
        if height >= 12 {
            assert!(
                surface.contains("Ready") || surface.contains("Responding"),
                "height {height}"
            );
        }
    }
}

#[test]
fn active_transcript_render_keeps_child_rows_out_of_main_and_renders_owner_navigation() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });
    for event in [
        TuiSubagentEvent::started(
            7,
            "reviewer",
            "review the active transcript",
            TuiExecutionState::ForegroundRunning,
        ),
        TuiSubagentEvent::reasoning(7, "child-reasoning-sentinel"),
        TuiSubagentEvent::text(7, "child-text-sentinel"),
        TuiSubagentEvent::tool_call(7, "child-call", "native::read", "child-input-sentinel"),
        TuiSubagentEvent::tool_result(7, "child-call", "child-result-sentinel", false),
        TuiSubagentEvent::error(7, SubagentErrorKind::Tool),
    ] {
        apply_subagent(&mut tui, event);
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Failed { id: 7 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(7, SubagentStatus::Failure, "child-final-sentinel"),
    );

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(
        !main.contains("Main · primary conversation"),
        "main provenance should be quiet: {main:?}"
    );
    assert!(
        main.contains("● Reviewer · review the active transcript"),
        "{main:?}"
    );
    for child_row in [
        "child-reasoning-sentinel",
        "child-text-sentinel",
        "child-input-sentinel",
        "child-result-sentinel",
        "Subagent tool execution failed.",
    ] {
        assert!(
            !main.contains(child_row),
            "duplicated {child_row:?}: {main:?}"
        );
    }
    // The card is the only place main learns why a delegated run failed, so its
    // final result belongs there even though the child's own rows do not.
    assert!(main.contains("child-final-sentinel"), "{main:?}");

    tui.select_transcript(TranscriptId::Subagent(7));
    // Ctrl+T shows the reasoning, Ctrl+O the tool output.
    tui.handle(Event::Key(Key::CtrlT));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let child = rendered_text(&renderer);
    assert!(child.contains("Subagent 7 · reviewer"), "{child:?}");
    assert!(
        child.contains("g select · m Main · h/l sibling"),
        "{child:?}"
    );
    assert!(
        child.contains("Subagent transcript · i to message · x to cancel"),
        "{child:?}"
    );
    assert!(!child.contains("Compose"), "{child:?}");
    for child_row in [
        "child-reasoning-sentinel",
        "child-text-sentinel",
        "child-input-sentinel",
        "child-result-sentinel",
        "Subagent tool execution failed.",
        "child-final-sentinel",
    ] {
        assert!(
            child.contains(child_row),
            "missing {child_row:?}: {child:?}"
        );
    }
}

#[test]
fn conversation_owns_the_first_row_under_every_notice_condition() {
    type ShowNotice = fn(&mut Tui<FakeEngine>);

    let notices: [(&str, ShowNotice); 3] = [
        ("Recovered failed prompt", |tui| {
            tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
                message: "Recovered failed prompt.".into(),
                presentation: TuiPresentation::new("provider", "model", "session #1"),
                history: Vec::new(),
                draft: Some("failed prompt".into()),
                resume_error: None,
                file_candidates: Vec::new(),
                palette_entries: Vec::new(),
            });
        }),
        ("danger", |tui| tui.set_dangerous_mode(true)),
        ("status-sentinel", |tui| tui.add_info("status-sentinel")),
    ];

    for (needle, show) in notices {
        let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 30)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 30,
        });
        show(&mut tui);

        renderer.render(tui.view()).unwrap();

        assert!(
            !rendered_line(&renderer, 0).contains(needle),
            "{needle:?} must not band the first row: {:?}",
            rendered_line(&renderer, 0)
        );
        let bottom = composer_bottom_row(&renderer);
        assert!(
            (bottom + 1..30).any(|row| rendered_line(&renderer, usize::from(row)).contains(needle)),
            "{needle:?} belongs to the bottom chrome, below composer bottom {bottom}: {:?}",
            rendered_text(&renderer)
        );
    }
}

#[test]
fn reserved_bottom_chrome_parks_the_composer_and_keeps_it_stable() {
    let height = 30_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 80, height });

    renderer.render(tui.view()).unwrap();
    let idle_top = composer_top_row(&renderer);
    let idle_bottom = composer_bottom_row(&renderer);
    assert!(
        idle_bottom + 3 < height,
        "the composer is parked above the reserved bottom chrome: bottom {idle_bottom}"
    );
    assert!(
        !rendered_line(&renderer, 0).contains('┌'),
        "the conversation owns the first row: {:?}",
        rendered_line(&renderer, 0)
    );

    tui.add_info("notice-sentinel");
    start_execution(&mut tui, 9, "explore");
    start_execution(&mut tui, 10, "plan");
    renderer.render(tui.view()).unwrap();

    assert_eq!(
        (composer_top_row(&renderer), composer_bottom_row(&renderer)),
        (idle_top, idle_bottom),
        "notices and the subagent tree must not move the composer"
    );
    let notice_row = rendered_row(&renderer, "notice-sentinel") as u16;
    let tree_row = rendered_row(&renderer, "Tab focus") as u16;
    let footer_row = rendered_row(&renderer, "model —") as u16;
    assert_eq!(
        footer_row, idle_bottom,
        "the metadata stays glued to the composer border"
    );
    assert!(
        idle_bottom < notice_row && notice_row < tree_row,
        "bottom chrome order below the composer is notice then tree: {notice_row} {tree_row}"
    );

    tui.handle(Event::Key(Key::Escape));
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        (composer_top_row(&renderer), composer_bottom_row(&renderer)),
        (idle_top, idle_bottom),
        "clearing a notice must not move the composer either"
    );
}

#[test]
fn tree_affordance_advertises_background_only_while_a_branch_runs_in_foreground() {
    let (width, height) = (100_u16, 30_u16);
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    start_execution(&mut tui, 9, "explore");

    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("Tab focus · Enter inspect · Ctrl+B background"),
        "a foreground branch can still be backgrounded: {:?}",
        rendered_text(&renderer)
    );

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::Backgrounded { id: 9 },
    });
    renderer.render(tui.view()).unwrap();
    let backgrounded = rendered_text(&renderer);
    assert!(
        backgrounded.contains("Tab focus · Enter inspect"),
        "focus and inspect always apply: {backgrounded:?}"
    );
    assert!(
        !backgrounded.contains("Ctrl+B"),
        "an already backgrounded branch must not advertise Ctrl+B: {backgrounded:?}"
    );

    start_execution(&mut tui, 10, "plan");
    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("Tab focus · Enter inspect · Ctrl+B background"),
        "a new foreground branch brings the hint back: {:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn bottom_chrome_flushes_the_subagent_tree_under_the_composer() {
    let (width, height) = (80_u16, 30_u16);
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    start_execution(&mut tui, 9, "explore");

    renderer.render(tui.view()).unwrap();
    let bottom = composer_bottom_row(&renderer);
    assert_eq!(
        rendered_row(&renderer, "Main") as u16,
        bottom + 1,
        "an inactive notice must not leave a dead row under the composer: {:?}",
        rendered_line(&renderer, usize::from(bottom + 1))
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        bottom,
        "the metadata keeps the composer's bottom border"
    );

    tui.add_info("notice-sentinel");
    renderer.render(tui.view()).unwrap();

    assert_eq!(
        composer_bottom_row(&renderer),
        bottom,
        "showing a notice must not move the composer"
    );
    assert_eq!(
        rendered_row(&renderer, "notice-sentinel") as u16,
        bottom + 1,
        "an active notice owns the row under the composer"
    );
    assert_eq!(
        rendered_row(&renderer, "Main") as u16,
        bottom + 2,
        "the tree follows the notice without a gap"
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        bottom,
        "the metadata keeps the composer's bottom border with a notice too"
    );
}

/// Composer border rows for a terminal whose gutter is not the wide default.
fn composer_border_rows(renderer: &RatatuiRenderer<TestBackend>, gutter: u16) -> (u16, u16) {
    let buffer = renderer.terminal().backend().buffer();
    let top = (0..buffer.area.height)
        .find(|row| buffer[(gutter, *row)].symbol() == "┌")
        .expect("composer top border should be rendered");
    let bottom = (top + 1..buffer.area.height)
        .find(|row| buffer[(gutter, *row)].symbol() == "└")
        .expect("composer bottom border should be rendered");
    (top, bottom)
}

fn docked_renderer(width: u16, height: u16) -> (RatatuiRenderer<TestBackend>, Tui<FakeEngine>) {
    let renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    (renderer, tui)
}

#[test]
fn composer_bottom_border_hosts_the_metadata_right_aligned() {
    let (width, height) = (100_u16, 24_u16);
    let (mut renderer, mut tui) = docked_renderer(width, height);

    renderer.render(tui.view()).unwrap();
    let (top, bottom) = composer_border_rows(&renderer, CHROME_GUTTER);

    assert_eq!(
        rendered_line(&renderer, usize::from(bottom)),
        format!(
            "    └{} model — · effort — · ctx — · tools hidden ^O · agens · ask ^⇧P · Ready ┘    ",
            "─".repeat(18)
        ),
        "the metadata is spliced into the composer border, one gap off the corner"
    );
    assert_eq!(
        rendered_text(&renderer).matches("model —").count(),
        1,
        "the metadata renders once, in the border only"
    );
    for row in bottom + 1..height {
        assert!(
            !rendered_line(&renderer, usize::from(row)).contains("Ready"),
            "no detached status row survives below the composer: row {row}"
        );
    }

    let idle_border_color = renderer.terminal().backend().buffer()[(CHROME_GUTTER, top)].fg;
    tui.begin_submission("prompt");
    renderer.render(tui.view()).unwrap();
    let (running_top, running_bottom) = composer_border_rows(&renderer, CHROME_GUTTER);
    assert_eq!((running_top, running_bottom), (top, bottom));
    assert!(
        !rendered_line(&renderer, usize::from(top)).contains(" running "),
        "the composer does not carry execution state: {:?}",
        rendered_line(&renderer, usize::from(top))
    );
    assert_eq!(
        renderer.terminal().backend().buffer()[(CHROME_GUTTER, top)].fg,
        idle_border_color,
        "the composer keeps its idle color while the turn runs"
    );
    assert!(
        rendered_line(&renderer, usize::from(bottom)).contains("model —"),
        "the bottom border keeps the metadata while running: {:?}",
        rendered_line(&renderer, usize::from(bottom))
    );
}

#[test]
fn border_metadata_drops_segments_as_the_composer_narrows() {
    // The declared shed order: the detail level, then effort, then the
    // directory, then the context, then the approval mode, leaving the model
    // and the turn outcome last.
    for (width, present, absent) in [
        (
            100_u16,
            "model — · effort — · ctx — · tools hidden ^O · agens · ask ^⇧P · Ready",
            "",
        ),
        (
            72,
            "model — · effort — · ctx — · agens · ask ^⇧P · Ready",
            "tools hidden",
        ),
        (56, "model — · ctx — · agens · ask ^⇧P · Ready", "effort —"),
        (52, "model — · ctx — · ask ^⇧P · Ready", "agens"),
        (40, "model — · ask ^⇧P · Ready", "ctx —"),
    ] {
        let (mut renderer, tui) = docked_renderer(width, 20);
        renderer.render(tui.view()).unwrap();
        let (_, bottom) = composer_border_rows(&renderer, CHROME_GUTTER);
        let line = rendered_line(&renderer, usize::from(bottom));

        assert!(
            line.contains(present),
            "width {width} keeps {present:?}: {line:?}"
        );
        assert!(
            absent.is_empty() || !line.contains(absent),
            "width {width} drops {absent:?}: {line:?}"
        );
        assert_eq!(
            line.chars().nth(usize::from(width - 2 - CHROME_GUTTER)),
            Some(' '),
            "width {width} keeps the metadata off the closing corner: {line:?}"
        );
        assert_eq!(
            line.chars().nth(usize::from(width - 1 - CHROME_GUTTER)),
            Some('┘'),
            "width {width} keeps the closing corner: {line:?}"
        );
    }
}

#[test]
fn a_border_too_narrow_for_the_metadata_falls_back_to_its_own_row() {
    let (width, height) = (24_u16, 20_u16);
    let (mut renderer, tui) = docked_renderer(width, height);

    renderer.render(tui.view()).unwrap();
    let (_, bottom) = composer_border_rows(&renderer, 0);
    let border = rendered_line(&renderer, usize::from(bottom));

    assert_eq!(
        border,
        format!("└{}┘", "─".repeat(usize::from(width - 2))),
        "a border too narrow for the shortest form stays undecorated"
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        height - 1,
        "the metadata falls back to its own row below the composer"
    );
}

#[test]
fn idle_bottom_chrome_leaves_at_most_two_rows_below_the_composer() {
    let height = 24_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 72, height });

    renderer.render(tui.view()).unwrap();
    let bottom = composer_bottom_row(&renderer);
    let gap = height
        .saturating_sub(1)
        .saturating_sub(bottom)
        .saturating_sub(1);

    assert!(
        gap <= 2,
        "the idle screen keeps at most two blank chrome rows: composer bottom {bottom}, gap {gap}"
    );
}

#[test]
fn elided_subagent_tree_keeps_a_running_branch_and_the_affordance_as_its_last_row() {
    let height = 14_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 72, height });
    for (id, agent) in [(9_u64, "explore"), (10, "plan"), (11, "build")] {
        start_execution(&mut tui, id, agent);
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "build".into(),
        event: TuiExecutionEvent::Completed { id: 11 },
    });

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("Plan #10"),
        "the elided tree keeps a running branch: {text:?}"
    );
    assert!(
        !text.contains("Build #11"),
        "a finished branch is elided before a running one: {text:?}"
    );
    assert_eq!(
        rendered_row(&renderer, "Tab to focus"),
        rendered_row(&renderer, "Plan #10") + 1,
        "the affordance survives elision as the last tree row: {text:?}"
    );
    assert!(
        rendered_row(&renderer, "Tab to focus") < usize::from(height - 1),
        "the elided tree stays inside its reserved band: {text:?}"
    );
    assert!(
        !text.contains("Main"),
        "the root is elided before a running branch: {text:?}"
    );
}

#[test]
fn elided_subagent_tree_reports_the_hidden_branch_count() {
    let height = 24_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 72, height });
    for (id, agent) in [(9_u64, "explore"), (10, "plan"), (11, "build")] {
        start_execution(&mut tui, id, agent);
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("+2 more · Tab to focus"),
        "the elision row states the hidden branch count and keeps the affordance: {text:?}"
    );
}

#[test]
fn bottom_chrome_degrades_without_panicking_on_small_terminals() {
    for (width, height, gutter) in [
        (1_u16, 1_u16, 0_u16),
        (1, 2, 0),
        (2, 1, 0),
        (4, 3, 0),
        (10, 7, 0),
        (20, 12, 0),
        (24, 14, 0),
        (40, 20, CHROME_GUTTER),
    ] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize { width, height });
        tui.add_info("notice-sentinel");
        start_execution(&mut tui, 9, "explore");

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        assert_eq!(
            text.chars().count(),
            usize::from(width) * usize::from(height),
            "{width}x{height}"
        );
        if height >= 2 {
            let buffer = renderer.terminal().backend().buffer();
            let top = (0..height)
                .find(|row| buffer[(gutter, *row)].symbol() == "┌")
                .unwrap_or_else(|| panic!("no composer top at {width}x{height}: {text:?}"));
            assert!(
                (top + 1..height).any(|row| buffer[(gutter, row)].symbol() == "└"),
                "the composer keeps priority over decorative chrome at {width}x{height}: {text:?}"
            );
            if width >= 2 {
                assert_eq!(
                    buffer[(width - 1 - gutter, top)].symbol(),
                    "┐",
                    "the gutter stays symmetric at {width}x{height}: {text:?}"
                );
            }
        }
    }
}

#[test]
fn active_transcript_render_keeps_terminal_child_renderable_after_expiry_and_switches_siblings() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for (id, agent, text) in [
        (7, "reviewer", "expired-child-sentinel"),
        (8, "writer", "sibling-child-sentinel"),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
        );
        apply_subagent(&mut tui, TuiSubagentEvent::text(id, text));
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 7 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(7, SubagentStatus::Success, "expired-final-sentinel"),
    );

    tui.select_transcript(TranscriptId::Subagent(7));
    tui.tick(Duration::from_secs(60));
    renderer.render(tui.view()).unwrap();
    let expired = rendered_text(&renderer);
    assert!(expired.contains("Subagent 7 · reviewer"), "{expired:?}");
    assert!(expired.contains("expired-child-sentinel"), "{expired:?}");
    assert!(expired.contains("expired-final-sentinel"), "{expired:?}");

    tui.handle(Event::Key(Key::Char('l')));
    renderer.render(tui.view()).unwrap();
    let sibling = rendered_text(&renderer);
    assert!(sibling.contains("Subagent 8 · writer"), "{sibling:?}");
    assert!(sibling.contains("sibling-child-sentinel"), "{sibling:?}");
    assert!(!sibling.contains("expired-child-sentinel"), "{sibling:?}");
}

#[test]
fn renderer_draws_the_file_picker_with_the_name_and_its_directory_column() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.add_info("conversation sentinel");
    tui.set_file_candidates(vec![
        "AGENTS.md".to_owned(),
        "crates/agens-cli/src/lib.rs".to_owned(),
        "crates/agens-tui/src/render.rs".to_owned(),
    ]);

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("conversation sentinel"));

    tui.handle(Event::Key(Key::Char('@')));
    renderer.render(tui.view()).unwrap();
    let picker = rendered_text(&renderer);

    assert!(picker.contains("files"), "{picker:?}");
    assert!(picker.contains("▌ AGENTS.md"), "{picker:?}");
    assert!(picker.contains("render.rs"), "{picker:?}");
    assert!(picker.contains("crates/agens-tui/src"), "{picker:?}");
    assert!(picker.contains("insert"), "{picker:?}");
    assert_eq!(
        rendered_column(&renderer, "crates/agens-cli/src"),
        rendered_column(&renderer, "crates/agens-tui/src"),
        "equal-width directories share the right-aligned column"
    );
    assert_eq!(
        cell_for_text(&renderer, "crates/agens-tui/src").fg,
        Color::Rgb(0x5c, 0x67, 0x73)
    );

    for character in "render".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    renderer.render(tui.view()).unwrap();
    let filtered = rendered_text(&renderer);
    assert!(filtered.contains("▌ render.rs"), "{filtered:?}");
    assert!(!filtered.contains("AGENTS.md"), "{filtered:?}");

    for character in "-missing".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("No matching files"),
        "{:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn the_file_picker_stays_panic_free_and_exact_on_narrow_terminals() {
    let mut tui = Tui::new(FakeEngine);
    tui.set_file_candidates(
        (0..40)
            .map(|index| format!("crates/agens-tui/src/module-{index:02}.rs"))
            .collect(),
    );
    tui.handle(Event::Key(Key::Char('@')));
    assert!(tui.view().file_picker.is_some());

    for (width, height) in [(1, 1), (2, 3), (8, 4), (12, 6), (34, 10)] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
        renderer.render(tui.view()).unwrap();
        assert_eq!(
            rendered_text(&renderer).chars().count(),
            usize::from(width) * usize::from(height),
            "file picker at {width}x{height}"
        );
    }
}

#[test]
fn bypass_is_compact_footer_metadata_instead_of_a_dedicated_notice() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.set_bypass(true);
    tui.begin_submission("prompt");

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert_eq!(rendered.matches("bypass").count(), 1, "{rendered:?}");
    assert!(!rendered.contains("BYPASS"), "{rendered:?}");
    // The mode is its own segment, so it no longer costs the turn its status.
    assert!(rendered.contains("bypass ^⇧P · Waiting"), "{rendered:?}");
}

#[test]
fn a_context_changed_outcome_shows_the_toggled_safety_state_without_a_further_keystroke() {
    for (needle, presentation) in [
        (
            "danger",
            TuiPresentation::new("provider", "model", "session #1").with_dangerous_mode(true),
        ),
        (
            "bypass",
            TuiPresentation::new("provider", "model", "session #1").with_bypass(true),
        ),
    ] {
        let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 30)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 30,
        });

        assert!(
            tui.apply_submission_outcome(TuiSubmissionOutcome::ContextChanged {
                message: format!("{needle} toggled."),
                presentation,
            })
            .is_none()
        );

        renderer.render(tui.view()).unwrap();

        assert!(
            rendered_text(&renderer).contains(needle),
            "{needle:?} must be visible right after the toggle: {:?}",
            rendered_text(&renderer)
        );
    }
}

const ERROR_COLOR: Color = Color::Rgb(0xf0, 0x71, 0x78);
const MUTED_COLOR: Color = Color::Rgb(0x5c, 0x67, 0x73);

#[test]
fn a_failed_turn_emphasizes_its_footer_status_instead_of_hiding_it_in_the_border_grey() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("request");
    tui.finish_submission(Err("provider: request rejected".into()));

    renderer.render(tui.view()).unwrap();

    let status = cell_for_text(&renderer, "Failed");
    assert_eq!(status.fg, ERROR_COLOR, "{:?}", rendered_text(&renderer));
    assert!(status.modifier.contains(Modifier::BOLD));
}

#[test]
fn a_failure_scrolled_out_of_the_viewport_still_announces_itself_above_the_composer() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 80,
        height: 20,
    });
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "filler-line\n".repeat(60),
    )));
    tui.finish_submission(Err("provider: request rejected".into()));

    tui.handle(Event::Key(Key::CtrlG));
    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(
        !rendered.contains("Action:"),
        "the error card has to be out of view for this assertion: {rendered:?}"
    );
    assert!(
        rendered.contains("provider: request rejected"),
        "the failure must stay perceptible without scrolling: {rendered:?}"
    );
    assert_eq!(
        cell_for_text(&renderer, "provider: request rejected").fg,
        ERROR_COLOR
    );
}

#[test]
fn a_failure_notice_is_not_painted_in_the_lowest_salience_style() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::Notice {
        text: "mcp: atlas failed to connect (unavailable)".into(),
        severity: agens_core::NoticeSeverity::Failure,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Notice {
        text: "session restored".into(),
        severity: agens_core::NoticeSeverity::Info,
    });

    renderer.render(tui.view()).unwrap();

    assert_eq!(cell_for_text(&renderer, "NOTICE").fg, ERROR_COLOR);
    assert_eq!(cell_for_text(&renderer, "mcp: atlas").fg, ERROR_COLOR);
    assert_eq!(cell_for_text(&renderer, "INFO").fg, MUTED_COLOR);
    assert_ne!(cell_for_text(&renderer, "session restored").fg, ERROR_COLOR);
}

#[test]
fn a_failed_subagent_card_shows_why_it_failed() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.tick(Duration::from_secs(5));
    start_execution(&mut tui, 10, "reviewer");
    tui.tick(Duration::from_secs(10));
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Failed { id: 10 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(
            10,
            SubagentStatus::Failure,
            "the reviewer ran out of context window",
        ),
    );

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("Failure · recent · 5s"), "{rendered:?}");
    assert!(
        rendered.contains("the reviewer ran out of context window"),
        "{rendered:?}"
    );
    assert_eq!(cell_for_text(&renderer, "the reviewer ran").fg, ERROR_COLOR);
}

#[test]
fn a_successful_subagent_card_keeps_its_result_out_of_the_transcript() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    start_execution(&mut tui, 11, "explore");
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(11, SubagentStatus::Success, "long successful summary body"),
    );

    renderer.render(tui.view()).unwrap();

    assert!(
        !rendered_text(&renderer).contains("long successful summary body"),
        "{:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn a_failed_tool_body_is_painted_apart_from_a_successful_one() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("request");
    for (call_id, output, is_error) in [
        ("ok-1", "success-body-sentinel", false),
        ("bad-1", "failure-body-sentinel", true),
    ] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: call_id.into(),
            name: "native::bash".into(),
            input: "run".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::bash".into(),
                raw: "run".into(),
            },
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: call_id.into(),
            output: output.into(),
            is_error,
        })
        .unwrap();
    }
    // Collapsed → Truncated: every settled call advances together.
    tui.handle(Event::Key(Key::CtrlO));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("success-body-sentinel"), "{rendered:?}");
    assert!(rendered.contains("failure-body-sentinel"), "{rendered:?}");
    assert_eq!(
        cell_for_text(&renderer, "success-body-sentinel").fg,
        MUTED_COLOR
    );
    assert_eq!(
        cell_for_text(&renderer, "failure-body-sentinel").fg,
        ERROR_COLOR
    );
}

#[test]
fn a_code_line_wider_than_its_panel_says_it_was_cut() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(46, 16)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("show");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```rust\nlet identificador_muy_largo = otra_funcion_con_nombre_extenso(argumento);\n```"
            .into(),
    )));

    renderer.render(tui.view()).unwrap();
    let code_row = transcript_rows(&renderer)
        .into_iter()
        .find(|row| row.contains("identificador_muy_largo"))
        .expect("the code panel should render its source line");

    assert!(code_row.contains('…'), "{code_row:?}");
    assert!(
        !code_row.contains("argumento"),
        "the tail is what got cut: {code_row:?}"
    );
}

#[test]
fn strikethrough_is_styled_instead_of_showing_its_tildes() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("show");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "deprecado: ~~STRIKESENTINEL~~ ahora".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("STRIKESENTINEL"), "{rendered:?}");
    assert!(!rendered.contains("~~"), "{rendered:?}");
    assert!(
        cell_for_text(&renderer, "STRIKESENTINEL")
            .modifier
            .contains(Modifier::CROSSED_OUT)
    );
}

#[test]
fn a_soft_break_reflows_instead_of_keeping_the_model_column_width() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("show");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "primera parte\nsegunda parte".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let rows = transcript_rows(&renderer);

    assert!(
        rows.iter()
            .any(|row| row.contains("primera parte segunda parte")),
        "{rows:?}"
    );
}

#[test]
fn html_keeps_its_text_and_drops_its_tags() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("show");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "un <b>HTMLSENTINEL</b> intercalado".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);

    assert!(rendered.contains("HTMLSENTINEL"), "{rendered:?}");
    assert!(!rendered.contains("<b>"), "{rendered:?}");
    assert!(!rendered.contains("</b>"), "{rendered:?}");
}

#[test]
fn a_wrapped_user_prompt_keeps_its_identity_rail_and_its_indent() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(46, 16)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission(
        "una linea de usuario deliberadamente larga con palabras extensas incomprensibles",
    );

    renderer.render(tui.view()).unwrap();
    let continuation = rendered_row(&renderer, "incomprensibles");
    let buffer = renderer.terminal().backend().buffer();

    assert_eq!(
        buffer[(ACCENT_COLUMN as u16, continuation as u16)].symbol(),
        "┃",
        "a continuation row stays inside the turn's rail"
    );
    let column = rendered_line(&renderer, continuation)
        .chars()
        .take_while(|character| character.is_whitespace() || *character == '┃')
        .count();
    assert_eq!(
        column, CONTENT_COLUMN,
        "a continuation row aligns under the prompt it continues"
    );
}

#[test]
fn a_wrapped_error_card_keeps_its_gutter_on_every_row() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(46, 18)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("show");
    tui.apply_conversation_event(ConversationEvent::Error {
        message: "provider: una falla con un mensaje deliberadamente largo que no entra".to_owned(),
        action: "Reintenta.".to_owned(),
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    let continuation = rendered_line(&renderer, rendered_row(&renderer, "entra"));

    assert!(
        continuation.trim_start().starts_with('│'),
        "{continuation:?}"
    );
}

#[test]
fn a_user_turn_is_a_band_that_spans_the_transcript_width() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(50, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("un pedido que no llega a llenar la fila");

    renderer.render(tui.view()).unwrap();
    let row = rendered_row(&renderer, "pedido");
    let buffer = renderer.terminal().backend().buffer();
    let banded = (0..buffer.area.width)
        .filter(|column| buffer[(*column, row as u16)].bg != Color::Reset)
        .count();

    assert_eq!(
        banded,
        usize::from(buffer.area.width) - CONTENT_COLUMN,
        "the band fills the row past the end of the prompt text"
    );
    assert_eq!(
        buffer[(ACCENT_COLUMN as u16, row as u16)].symbol(),
        "┃",
        "the rail carries the same meaning where no background is drawn"
    );
}

#[test]
fn only_the_user_turn_is_banded() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(50, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("pedido");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "ANSWER_SENTINEL".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let answer = rendered_row(&renderer, "ANSWER_SENTINEL");
    let buffer = renderer.terminal().backend().buffer();

    assert!(
        (0..buffer.area.width).all(|column| buffer[(column, answer as u16)].bg == Color::Reset),
        "prose carries no band, so the band means one thing only"
    );
}

#[test]
fn the_transcript_greys_run_from_prose_to_tool_header_to_tool_output() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(70, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("go");
    tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
        call_id: "call-1".into(),
        name: "native::read".into(),
        input: "{\"path\":\"PATH_SENTINEL\"}".into(),
        parsed: agens_core::ToolInput::Read {
            path: "PATH_SENTINEL".into(),
        },
    });
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "call-1".into(),
        name: "native::read".into(),
        input: "{\"path\":\"PATH_SENTINEL\"}".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "call-1".into(),
        content: "OUTPUT_SENTINEL".into(),
        is_error: false,
    }));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "PROSE_SENTINEL".into(),
    )));
    tui.handle(Event::Key(Key::CtrlO));

    renderer.render(tui.view()).unwrap();

    let luminance = |text: &str| -> u32 {
        match cell_for_text(&renderer, text).fg {
            Color::Rgb(red, green, blue) => u32::from(red) + u32::from(green) + u32::from(blue),
            other => panic!("{text} is painted {other:?}"),
        }
    };

    assert!(
        luminance("PROSE_SENTINEL") > luminance("path="),
        "the answer reads louder than what the agent ran"
    );
    assert!(
        luminance("path=") > luminance("OUTPUT_SENTINEL"),
        "what the agent ran reads louder than what it printed"
    );
}

#[test]
fn the_footer_answers_its_questions_at_every_real_terminal_width() {
    for width in [80_u16, 120, 200] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, 14)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.apply_presentation(
            TuiPresentation::new("openai-api", "gpt-5.6-sol", "session #1")
                .with_effort("high")
                .with_context_window(Some(200_000)),
        );
        tui.set_project("/home/iperez/dev/personal/deep/nested/workspace/agens");
        tui.set_repository_probe(std::sync::Arc::new(|| {
            Some(RepositoryStatus {
                branch: Some("feat/agn-114".to_owned()),
                changed_files: 3,
                insertions: 120,
                deletions: 8,
            })
        }));
        tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
            input_tokens: Some(70_000),
            output_tokens: Some(1_000),
            total_tokens: Some(71_000),
            context_window: Some(200_000),
        }));

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        // What survives at 80 columns is the floor every wider terminal keeps.
        for datum in ["gpt-5.6-sol", "71k/200k", "36%", "ask ^⇧P"] {
            assert!(
                text.contains(datum),
                "width {width} lost {datum:?}: {text:?}"
            );
        }
        // A deep path never spends the footer's budget on its ancestors.
        assert!(
            text.contains("/agens") && !text.contains("/personal/deep"),
            "width {width}: {text:?}"
        );
        if width >= 120 {
            assert!(text.contains("feat/agn-114"), "width {width}: {text:?}");
            assert!(text.contains("+120"), "width {width}: {text:?}");
        }
    }
}

#[test]
fn a_subagent_card_names_the_model_and_effort_it_runs_on() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 16)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("delegate");
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::BackgroundStarted { id: 1 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started_on(
            1,
            "explore",
            "sweep the repository",
            TuiExecutionState::BackgroundRunning,
            Some("gpt-5.6-sol"),
            Some("high"),
        ),
    );

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("gpt-5.6-sol"), "{text:?}");
    assert!(text.contains("high"), "{text:?}");
}

#[test]
fn a_background_subagent_keeps_the_surface_repainting_after_its_turn_ends() {
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("delegate");
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::BackgroundStarted { id: 1 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(1, "explore", "sweep", TuiExecutionState::BackgroundRunning),
    );
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));

    assert!(
        !tui.view().running,
        "the parent turn is what finished, not the delegation"
    );
    assert!(
        tui.has_live_work(),
        "a running background subagent still owes the reader a moving clock"
    );

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::Completed { id: 1 },
    });

    assert!(
        !tui.has_live_work(),
        "nothing running means nothing to repaint for"
    );
}

#[test]
fn the_turn_status_row_is_separated_from_what_the_agent_just_said() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(70, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("ask");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "ANSWER_SENTINEL".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let answer = rendered_row(&renderer, "ANSWER_SENTINEL");
    let status = rendered_row(&renderer, "Responding…");

    assert_eq!(
        status,
        answer + 2,
        "a blank row separates the answer from the row reporting on it"
    );
}

#[test]
fn consecutive_tool_rows_pack_without_blank_rows_between_them() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(90, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("go");
    for index in 0..3 {
        let call_id = format!("call-{index}");
        let name = format!("mcp_server_tool_{index}");
        tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
            call_id: call_id.clone(),
            name: name.clone(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Other {
                name: name.clone(),
                raw: "{}".into(),
            },
        });
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: call_id.clone(),
            name,
            input: "{}".into(),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: call_id.clone(),
            content: "ok".into(),
            is_error: false,
        }));
        tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
            call_id,
            duration: Some(Duration::from_millis(9)),
            result: ToolResultState::Success,
        });
    }

    renderer.render(tui.view()).unwrap();
    let first = rendered_row(&renderer, "mcp_server_tool_0");
    let last = rendered_row(&renderer, "mcp_server_tool_2");
    let blank_rows = (first..=last)
        .filter(|row| rendered_line(&renderer, *row).trim().is_empty())
        .count();

    assert!(last > first, "the three calls render in order");
    assert_eq!(
        blank_rows, 0,
        "a run of one-line tool rows spends no rows saying nothing"
    );
}

#[test]
fn a_long_turn_reports_its_time_in_units_a_reader_sizes_up() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(90, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("ask");
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(90_000),
        output_tokens: Some(359),
        total_tokens: Some(90_359),
        context_window: Some(200_000),
    }));
    tui.tick(Duration::from_secs(253));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("4m 13s"), "{text:?}");
    assert!(!text.contains("253s"), "{text:?}");
    assert!(text.contains("90.4k tok"), "{text:?}");
    assert!(!text.contains("90359"), "{text:?}");
}

#[test]
fn typed_input_sits_in_the_same_column_as_the_prose_above_it() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(60, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 60,
        height: 14,
    });
    tui.begin_submission("ask");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "ANSWER_SENTINEL".into(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    for character in "INPUT_SENTINEL".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();

    assert_eq!(
        rendered_column(&renderer, "INPUT_SENTINEL"),
        rendered_column(&renderer, "ANSWER_SENTINEL"),
        "what the user types lines up with what the agent said"
    );
}

// --- `native::ask_user` two-column contextual layout ---------------------------

/// Terminal width whose overlay resolves wide enough for two columns.
const ASK_USER_WIDE_TERMINAL: u16 = 100;
/// Terminal width whose overlay is forced to stack the context below the list.
const ASK_USER_NARROW_TERMINAL: u16 = 56;

fn ask_user_option(
    id: &str,
    label: &str,
    explanation: &str,
    context: Option<&str>,
) -> AskUserOption {
    AskUserOption::new(
        id,
        label,
        Some(explanation.to_owned()),
        context.map(str::to_owned),
    )
}

fn ask_user_request_with_context() -> AskUserRequest {
    AskUserRequest::new(
        Some("Pick a rollout".to_owned()),
        vec![AskUserQuestion::new(
            "plan",
            "How should the migration land?",
            Some("Both options keep the old table readable.".to_owned()),
            AskUserMode::Single,
            vec![
                ask_user_option(
                    "big-bang",
                    "BIGBANG_LABEL",
                    "BIGBANG_EXPLAIN one shot",
                    Some("BIGBANG_CONTEXT cuts over in a single deploy window."),
                ),
                ask_user_option(
                    "phased",
                    "PHASED_LABEL",
                    "PHASED_EXPLAIN two steps",
                    Some("PHASED_CONTEXT dual writes first, then a backfill."),
                ),
            ],
            false,
            false,
            false,
        )],
    )
    .expect("a bounded single question forms a valid request")
}

fn three_question_ask_user_request() -> AskUserRequest {
    let question = |id: &str| {
        AskUserQuestion::new(
            id,
            format!("PROMPT_{id}"),
            None,
            AskUserMode::Single,
            vec![
                ask_user_option("a", &format!("{id}_A"), "first", Some("CTX_A")),
                ask_user_option("b", &format!("{id}_B"), "second", Some("CTX_B")),
            ],
            true,
            true,
            false,
        )
    };
    AskUserRequest::new(None, vec![question("q1"), question("q2"), question("q3")])
        .expect("three bounded questions form a valid request")
}

fn open_ask_user(
    width: u16,
    height: u16,
    request: AskUserRequest,
) -> (Tui<FakeEngine>, RatatuiRenderer<TestBackend>) {
    let mut tui = Tui::new(FakeEngine);
    tui.open_ask_user(1, request);
    let renderer = rendered_at(&mut tui, width, height);
    (tui, renderer)
}

/// Re-renders the same interaction into a terminal of a different size.
///
/// `TestBackend` cannot be resized behind the renderer, so the frame is
/// rebuilt; the [`Tui`] is the same one, which is what the assertion about
/// preserved state is actually about.
fn rendered_at(tui: &mut Tui<FakeEngine>, width: u16, height: u16) -> RatatuiRenderer<TestBackend> {
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    tui.handle(Event::Resize { width, height });
    renderer.render(tui.view()).unwrap();
    renderer
}

#[test]
fn ask_user_wide_layout_puts_context_beside_the_options_and_follows_the_highlight() {
    let (mut tui, mut renderer) =
        open_ask_user(ASK_USER_WIDE_TERMINAL, 30, ask_user_request_with_context());

    let text = rendered_text(&renderer);
    assert!(text.contains("BIGBANG_LABEL"), "{text:?}");
    assert!(text.contains("BIGBANG_EXPLAIN"), "{text:?}");
    assert!(text.contains("PHASED_LABEL"), "{text:?}");
    assert!(
        text.contains("BIGBANG_CONTEXT"),
        "the highlighted option's context is shown: {text:?}"
    );
    assert!(
        !text.contains("PHASED_CONTEXT"),
        "only the highlighted option's context is shown: {text:?}"
    );
    assert!(
        rendered_column(&renderer, "BIGBANG_CONTEXT") > rendered_column(&renderer, "BIGBANG_LABEL"),
        "context sits in the right column, beside the options"
    );
    assert_eq!(
        rendered_row(&renderer, "BIGBANG_CONTEXT"),
        rendered_row(&renderer, "BIGBANG_LABEL"),
        "the first context row is level with the first option row"
    );

    tui.handle(Event::Key(Key::Down));
    renderer.render(tui.view()).unwrap();

    let text = rendered_text(&renderer);
    assert!(
        text.contains("PHASED_CONTEXT"),
        "moving the highlight changes the context pane: {text:?}"
    );
    assert!(!text.contains("BIGBANG_CONTEXT"), "{text:?}");
}

#[test]
fn ask_user_narrow_layout_stacks_context_below_the_options_with_a_visible_affordance() {
    let (_tui, renderer) = open_ask_user(
        ASK_USER_NARROW_TERMINAL,
        30,
        ask_user_request_with_context(),
    );

    let text = rendered_text(&renderer);
    assert!(text.contains("BIGBANG_LABEL"), "{text:?}");
    assert!(
        text.contains("BIGBANG_CONTEXT"),
        "context stays reachable when the overlay is too narrow for two columns: {text:?}"
    );
    assert!(
        rendered_row(&renderer, "BIGBANG_CONTEXT") > rendered_row(&renderer, "BIGBANG_LABEL"),
        "context moves below the option list"
    );
    assert!(
        text.contains("context"),
        "the stacked section names itself so the reader knows it can be scrolled: {text:?}"
    );
    assert!(
        text.contains("pgup/pgdn"),
        "the keys that reach it are on screen: {text:?}"
    );
}

#[test]
fn ask_user_resizing_across_the_layout_threshold_preserves_every_interaction_state() {
    let (mut tui, _wide) = open_ask_user(
        ASK_USER_WIDE_TERMINAL,
        30,
        three_question_ask_user_request(),
    );

    tui.handle(Event::Key(Key::Tab));
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::Enter));
    for character in "oOTHER_TEXT".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    tui.handle(Event::Key(Key::Enter));
    tui.handle(Event::Key(Key::Char('n')));
    for character in "NOTE_TEXT".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    let before = tui.ask_user_snapshot().expect("ask-user still open");
    assert_eq!(before.question_index, 1);
    assert_eq!(before.selected, vec![1]);
    assert_eq!(before.other, "OTHER_TEXT");
    assert_eq!(before.note, "NOTE_TEXT");

    let renderer = rendered_at(&mut tui, ASK_USER_NARROW_TERMINAL, 30);
    assert_eq!(
        tui.ask_user_snapshot().expect("ask-user still open"),
        before,
        "no interaction state may be derived from terminal width"
    );
    let narrow = rendered_text(&renderer);
    assert!(narrow.contains("OTHER_TEXT"), "{narrow:?}");
    assert!(narrow.contains("NOTE_TEXT"), "{narrow:?}");
    assert!(narrow.contains("CTX_B"), "{narrow:?}");

    let renderer = rendered_at(&mut tui, ASK_USER_WIDE_TERMINAL, 30);
    assert_eq!(
        tui.ask_user_snapshot().expect("ask-user still open"),
        before,
        "returning to the wide layout restores nothing because nothing was lost"
    );
    let wide = rendered_text(&renderer);
    assert!(wide.contains("OTHER_TEXT"), "{wide:?}");
    assert!(wide.contains("q2_B"), "{wide:?}");
}

fn long_context_request() -> AskUserRequest {
    let context = (0..60)
        .map(|index| format!("CTXLINE{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let second = (0..60)
        .map(|index| format!("BCTXLINE{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    AskUserRequest::new(
        None,
        vec![AskUserQuestion::new(
            "q1",
            "long context",
            None,
            AskUserMode::Single,
            vec![
                ask_user_option("a", "A_LABEL", "first", Some(&context)),
                ask_user_option("b", "B_LABEL", "second", Some(&second)),
            ],
            false,
            false,
            false,
        )],
    )
    .expect("a bounded question forms a valid request")
}

#[test]
fn ask_user_context_pane_scrolls_by_keyboard_and_end_leaves_a_reachable_last_page() {
    let (mut tui, mut renderer) = open_ask_user(ASK_USER_WIDE_TERMINAL, 30, long_context_request());
    let text = rendered_text(&renderer);
    assert!(text.contains("CTXLINE00"), "{text:?}");

    tui.handle(Event::Key(Key::PageDown));
    renderer.render(tui.view()).unwrap();
    let scrolled = rendered_text(&renderer);
    assert!(!scrolled.contains("CTXLINE00"), "{scrolled:?}");

    tui.handle(Event::Key(Key::End));
    renderer.render(tui.view()).unwrap();
    let bottom = rendered_text(&renderer);
    assert!(
        bottom.contains("CTXLINE59"),
        "End reaches the last context row: {bottom:?}"
    );

    tui.handle(Event::Key(Key::PageUp));
    renderer.render(tui.view()).unwrap();
    let stepped_back = rendered_text(&renderer);
    assert_ne!(
        stepped_back, bottom,
        "a single PageUp after End must move the pane, not walk back from a sentinel"
    );
    assert!(
        !stepped_back.contains("CTXLINE59"),
        "one page back from the bottom leaves the last row: {stepped_back:?}"
    );

    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("CTXLINE00"));
}

#[test]
fn ask_user_context_scroll_resets_between_options_but_survives_an_action_row_move() {
    let (mut tui, mut renderer) = open_ask_user(ASK_USER_WIDE_TERMINAL, 30, long_context_request());
    tui.handle(Event::Key(Key::PageDown));
    let scrolled = tui.ask_user_snapshot().unwrap().context_scroll;
    assert!(scrolled > 0);

    tui.handle(Event::Key(Key::Down));
    assert_eq!(
        tui.ask_user_snapshot().unwrap().context_scroll,
        0,
        "a different option shows different context, so its scroll starts at the top"
    );

    tui.handle(Event::Key(Key::PageDown));
    let before_action_rows = tui.ask_user_snapshot().unwrap().context_scroll;
    assert_eq!(before_action_rows, scrolled);

    tui.handle(Event::Key(Key::Down));
    renderer.render(tui.view()).unwrap();
    let on_submit = rendered_text(&renderer);
    assert_eq!(
        tui.ask_user_snapshot().unwrap().context_scroll,
        before_action_rows,
        "walking down to the action rows does not change what the context pane shows"
    );
    assert!(
        on_submit.contains("BCTXLINE1"),
        "the last highlighted option keeps the pane while the cursor is on an action row: \
         {on_submit:?}"
    );

    tui.handle(Event::Key(Key::Down));
    assert_eq!(
        tui.ask_user_snapshot().unwrap().context_scroll,
        before_action_rows,
        "moving between action rows does not change what the context pane shows"
    );
}

#[test]
fn ask_user_context_keeps_diagram_rows_and_truncates_an_unbreakable_line() {
    let context = format!(
        "  ┌──────────┐\n  │ small    │\n  └──────────┘\nWIDE ┌{0}┐ ───▶ ┌{0}┐\nRULE{1}",
        "─".repeat(30),
        "─".repeat(72)
    );
    let request = AskUserRequest::new(
        None,
        vec![AskUserQuestion::new(
            "q1",
            "diagram",
            None,
            AskUserMode::Single,
            vec![ask_user_option("a", "A_LABEL", "first", Some(&context))],
            false,
            false,
            false,
        )],
    )
    .expect("a bounded question forms a valid request");
    let (_tui, renderer) = open_ask_user(ASK_USER_WIDE_TERMINAL, 30, request);

    let text = rendered_text(&renderer);
    assert!(
        text.contains("┌──────────┐"),
        "a diagram row that fits is painted verbatim: {text:?}"
    );
    assert!(
        text.contains("│ small    │"),
        "interior spacing is preserved: {text:?}"
    );
    let wide_row = rendered_row(&renderer, "WIDE");
    assert!(
        rendered_line(&renderer, wide_row).contains('…'),
        "a diagram row wider than the column is cut even though it has spaces \
         in it — word-wrapping a diagram misaligns every row below it: {:?}",
        rendered_line(&renderer, wide_row)
    );
    assert!(
        !rendered_line(&renderer, wide_row + 1).contains("───▶"),
        "the cut row's remainder never re-flows onto the next row: {:?}",
        rendered_line(&renderer, wide_row + 1)
    );

    let rule_row = rendered_row(&renderer, "RULE");
    assert!(
        rendered_line(&renderer, rule_row).contains('…'),
        "an unbreakable row wider than the column is cut: {:?}",
        rendered_line(&renderer, rule_row)
    );
    assert!(
        !rendered_line(&renderer, rule_row + 1).contains("──────"),
        "the remainder is dropped, never re-flowed onto the next row: {:?}",
        rendered_line(&renderer, rule_row + 1)
    );
}

#[test]
fn ask_user_header_reports_completion_and_names_the_question_that_blocks_submission() {
    let (mut tui, mut renderer) = open_ask_user(
        ASK_USER_WIDE_TERMINAL,
        30,
        three_question_ask_user_request(),
    );
    tui.handle(Event::Key(Key::Enter));
    renderer.render(tui.view()).unwrap();
    let answered_one = rendered_text(&renderer);
    assert!(answered_one.contains("1 of 3 answered"), "{answered_one:?}");

    for _ in 0..2 {
        tui.handle(Event::Key(Key::Down));
    }
    tui.handle(Event::Key(Key::Enter));
    renderer.render(tui.view()).unwrap();

    let blocked = rendered_text(&renderer);
    assert!(
        blocked.contains("answer question 2 first"),
        "an incomplete submission names the question that blocks it: {blocked:?}"
    );
    assert!(tui.ask_user_snapshot().is_some(), "nothing was submitted");
}

/// The settled render cache is only bypassed when something actually moved, so
/// an ask-user key that changes nothing has to say so. Scroll keys are the ones
/// at risk: before the pane's real extent was measured, `End` always reported a
/// change because it stored a sentinel no pane could ever be as tall as.
#[test]
fn ask_user_scroll_keys_report_unchanged_when_the_context_pane_cannot_move() {
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: ASK_USER_WIDE_TERMINAL,
        height: 30,
    });
    tui.open_ask_user(1, ask_user_request_with_context());

    for key in [Key::End, Key::PageDown, Key::Home, Key::PageUp] {
        assert_eq!(
            tui.handle(Event::Key(key)),
            Action::Unchanged,
            "{key:?} on a context that fits its pane changes nothing a reader can see"
        );
    }

    let mut scrollable = Tui::new(FakeEngine);
    scrollable.handle(Event::Resize {
        width: ASK_USER_WIDE_TERMINAL,
        height: 30,
    });
    scrollable.open_ask_user(1, long_context_request());
    assert_eq!(scrollable.handle(Event::Key(Key::End)), Action::Render);
    assert_eq!(
        scrollable.handle(Event::Key(Key::End)),
        Action::Unchanged,
        "a second End is already at the bottom"
    );
}

fn single_context_request(context: &str) -> AskUserRequest {
    AskUserRequest::new(
        None,
        vec![AskUserQuestion::new(
            "q1",
            "context shape",
            None,
            AskUserMode::Single,
            vec![ask_user_option("a", "A_LABEL", "first", Some(context))],
            false,
            false,
            false,
        )],
    )
    .expect("a bounded question forms a valid request")
}

#[test]
fn ask_user_context_freezes_ascii_diagrams_and_still_wraps_ordinary_prose() {
    let context = "\
ASCII |   ingest    |   -->    |   transform |   -->    |    store    |
+--------+   +-----------+   +---------+   +----------+   +----------+
FITS |  a  |  -->  |  b  |
PROSE the well-known trade-off here is a pipe | and a hyphen - in ordinary \
prose that has to keep wrapping across several rows of the pane instead of \
being frozen and cut";
    let (_tui, renderer) =
        open_ask_user(ASK_USER_WIDE_TERMINAL, 30, single_context_request(context));

    let ascii_row = rendered_row(&renderer, "ASCII");
    let ascii_line = rendered_line(&renderer, ascii_row);
    assert!(
        ascii_line.contains("|   ingest    |"),
        "an ASCII diagram's interior spacing is load-bearing and must survive \
         verbatim up to the cut: {ascii_line:?}"
    );
    assert!(
        ascii_line.contains('…'),
        "an over-wide ASCII diagram row is cut, not re-flowed: {ascii_line:?}"
    );
    assert!(
        !rendered_line(&renderer, ascii_row + 1).contains("store"),
        "the cut row's remainder never re-flows onto the next row: {:?}",
        rendered_line(&renderer, ascii_row + 1)
    );

    let rule_row = rendered_row(&renderer, "+--------+");
    assert!(
        rendered_line(&renderer, rule_row).contains('…'),
        "a pure-ASCII rule of boxes is a drawing too: {:?}",
        rendered_line(&renderer, rule_row)
    );

    let fits_line = rendered_line(&renderer, rendered_row(&renderer, "FITS"));
    assert!(
        fits_line.contains("|  a  |  -->  |  b  |"),
        "a drawing that fits is painted exactly as authored: {fits_line:?}"
    );

    let prose_row = rendered_row(&renderer, "PROSE");
    let prose_line = rendered_line(&renderer, prose_row);
    assert!(
        !prose_line.contains('…'),
        "prose that merely contains a hyphen and a pipe is not a drawing and \
         must wrap, not freeze: {prose_line:?}"
    );
    let text = rendered_text(&renderer);
    assert!(
        text.contains("instead of"),
        "the tail of the wrapped paragraph is still on screen: {text:?}"
    );
}

#[test]
fn ask_user_context_wraps_wide_glyphs_by_display_width_without_losing_characters() {
    let body = "設計上の判断をここに書き並べておくための長い日本語の段落である".repeat(3);
    let (_tui, renderer) = open_ask_user(
        ASK_USER_WIDE_TERMINAL,
        30,
        single_context_request(&format!("始{body}終")),
    );

    let text = rendered_text(&renderer);
    assert!(text.contains('始'), "the head of the paragraph is shown");
    assert!(
        text.contains('終'),
        "text with no ASCII space still has to wrap; truncating it to one row \
         puts characters in no buffer that any keypress can reach: {text:?}"
    );
    assert!(
        rendered_row(&renderer, "終") > rendered_row(&renderer, "始"),
        "a paragraph wider than the pane occupies more than one row"
    );
}

#[test]
fn ask_user_context_wraps_double_width_emoji_without_dropping_the_tail() {
    let (_tui, renderer) = open_ask_user(
        ASK_USER_WIDE_TERMINAL,
        30,
        single_context_request(&format!("START{}END", "🙂".repeat(160))),
    );

    let text = rendered_text(&renderer);
    assert!(text.contains("START"), "{text:?}");
    assert!(
        text.contains("END"),
        "wrapping measured in characters rather than display columns builds \
         rows twice as wide as the pane, and everything past the pane's width \
         is silently clipped: {text:?}"
    );
}

/// The scroll-position row is reserved out of the pane only when the pane is
/// tall enough to spare it. Painting it unconditionally writes one row below
/// the pane — onto the footer, or onto the overlay's own border — which no
/// panic ever reports.
fn assert_ask_user_frame_is_not_corrupted(renderer: &RatatuiRenderer<TestBackend>, label: &str) {
    let height = renderer.terminal().backend().buffer().area.height;
    for row in 0..height {
        let line = rendered_line(renderer, usize::from(row));
        let footer = line.contains("↑↓ move") || line.contains("esc cancel");
        let position = line.contains(" of ") && line.contains("pgup/pgdn");
        assert!(
            !(footer && position),
            "{label}: the context position row escaped its pane onto the \
             footer: {line:?}"
        );
        assert!(
            line.matches("pgup/pgdn").count() <= 1,
            "{label}: the context position row was painted over another row: {line:?}"
        );
    }
}

#[test]
fn ask_user_overlay_never_panics_or_corrupts_its_frame_at_degenerate_sizes() {
    let long = (0..40)
        .map(|index| format!("CTXLINE{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    for (width, height) in [
        (1, 1),
        (8, 4),
        (34, 10),
        (12, 3),
        (200, 3),
        (100, 5),
        (100, 6),
        (100, 7),
        (100, 8),
        (100, 9),
        (56, 7),
        (56, 16),
    ] {
        let (_tui, renderer) = open_ask_user(width, height, ask_user_request_with_context());
        assert_ask_user_frame_is_not_corrupted(&renderer, &format!("{width}x{height} short"));

        let (_tui, renderer) = open_ask_user(width, height, single_context_request(&long));
        assert_ask_user_frame_is_not_corrupted(&renderer, &format!("{width}x{height} long"));
    }
}
