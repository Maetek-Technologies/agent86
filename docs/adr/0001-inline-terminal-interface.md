# 0001. The interactive interface renders inline, in Claude Code's style

## Status

Accepted — 2026-10-26

## Context

Python's agent86 has two interactive surfaces that share one command registry (`tui/commands.py`):

- **The plain loop** (`ui/repl.py`): ported in stage 9, part 1.
- **The full-screen Textual app** (`tui/app.py` and its widgets and screens, about 4,000 lines): Python's default whenever `[ui] tui` is on and stdin and stdout are terminals.

The full-screen app runs on the terminal's alternate screen. That has costs for anyone using it:

- The conversation lives in the app's own scroll view, so the terminal's scrollback, search, and text selection don't work on it.
- When the app exits, the conversation disappears from the terminal.
- Tool blocks and Markdown replies can change after they are written, which means the whole transcript is re-rendered (`Agent86App._rerender`).

The user wants the interface to feel like Claude Code. Claude Code is not a full-screen app. It renders inline:

- Finished output (prompts, answers, tool calls) goes into the terminal's normal scrollback.
- Only a small live region at the bottom is redrawn: the streaming answer, a spinner, completion menus, permission prompts, the bordered input box, and a status line.

That inline model is what makes it feel the way it does.

The Rust options considered:

1. **Full-screen ratatui:** a faithful copy of Python's Textual app, with the same drawbacks.
2. **A line editor only, such as `reedline`:** good editing and menus, but no live region while a turn streams, so no spinner, live status, or inline approval panel.
3. **Inline ratatui on crossterm:** a fixed-height live region at the bottom (`Viewport::Inline`), with finished lines written into native scrollback above it (`Terminal::insert_before`). OpenAI's Codex CLI uses this pattern.
4. **A desktop or web GUI, such as Tauri or egui:** a different product, not a terminal tool.

## Decision

**Ruling:** The stage 9 interface renders inline, with ratatui and crossterm, in Claude Code's visual style. It keeps Python's behavior and covers every screen of Python's full-screen app in stages.

What stays Python's (the compatibility contract):

- **Commands:**
  - The command registry, its names and order.
  - Every message a command prints.
  - Startup notes, MCP notes, `@path` mention expansion and its refusals, the turn summary line, and the error hint (`see agent86 trace show -s <id>`).
- **Approvals:**
  - Approval semantics (`ask`/`auto`/`deny`, decline when nobody can answer, deny on shutdown) and the preview shown (the argument summary plus the tool's detail).
- **Status line:**
  - Its segments and the order it sheds them as the terminal narrows (`ui/status.py`, `status_footer.py`).
- **Files:**
  - The session database, the prompt history file, the config files and how they are written back, and the keyring layout.
- **Cancellation:**
  - Cooperative cancellation at safe points, the `[cancelled]` notice, and the saved and traced outcome.
- **Surface selection:**
  - `[ui] tui`, `--plain`, `AGENT86_PLAIN`, and the terminal checks, plus the plain-loop fallback and its note.

What changes (presentation only):

- **Where the conversation lives:** in native scrollback, not on the alternate screen. Finished entries are written once and never re-rendered.
- **Visual vocabulary:** Claude Code's. That means `>` for user prompts, `⏺` for replies and tool calls, `⎿` under a tool call for its result, a spinner line with elapsed time and "esc to interrupt", a bordered input box, and numbered choices in panels.
- **Python's modal screens become inline panels.** Each one is drawn in the live region in place of the input box. These are the approval, mode picker, model picker, session picker, provider manager, catalog picker, key entry, connection test, save-diff, MCP manager, MCP form, and MCP test screens.
- **Expanding tool blocks:** Python expands tool blocks in place (Ctrl+O). Inline output can't be changed once written, so Ctrl+O opens a full-screen transcript view, with every tool block expanded, until Esc.
- **Markdown:** Python re-renders a finished reply as Markdown. Here, Markdown is rendered block by block as the reply streams: a block goes to scrollback once it is complete, and the unfinished tail stays in the live region.

Where the interface can't run, it falls back to the plain loop with Python's note, `TUI could not start (<reason>); using plain REPL.`. That happens when raw mode or the inline viewport can't be set up. The main example is Git Bash's mintty without winpty: Rust's `is_terminal` reports its pipes as terminals, but they are not consoles.

## Consequences

- **This is a deliberate difference from the oracle.** `docs/PROJECT_PLAN.md` records it under "Known REPL differences from Python". Each later difference introduced by this design goes on the same list.
- **The plain loop remains the strict-compatibility surface.** It covers `--plain`, piped input, and terminals that can't host the interface, and `tests/repl.rs` keeps pinning it.
- **The scrollback is the terminal's.** Its search, selection, and copy work, and the conversation stays on screen after exit. The cost: text already in scrollback is not re-wrapped by the interface when the window is resized (the terminal reflows it or not), and `/clear` clears the screen rather than editing history.
- **The harness gains a structured turn-event stream.** It covers text, tool start, tool result, notice, and phase. It sits next to the existing text `emit`, so the interface doesn't need to parse `[tool] ...` lines the way Python's `turn_bridge` does. `run` and the plain loop keep printing exactly what they print now.
- **More code than Textual gives Python for free:** a multi-line input editor, menus, panels, a Markdown-to-terminal renderer, and a full-screen transcript view. `tui-textarea` targets ratatui 0.29, so the editor is our own.
- **Dependencies:** `ratatui` 0.30 (its MSRV is 1.88, the same as ours), `crossterm` 0.29, and `pulldown-cmark` 0.13; `toml_edit` arrives with config write-back. All of them build on the current MSRV.
- **Testing:** the interface is built so that its state machine and rendering can be tested without a terminal. Rendering is tested with ratatui's `TestBackend`, and the app is driven by synthetic key events against the mock provider. What only a real terminal shows is covered by a manual checklist per slice.
- **Design and slicing:** `docs/INTERFACE.md` holds the design and the slice plan.
