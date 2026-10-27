# Interactive interface: design and plan (stage 9, parts 2 onward)

This is the design for the interactive interface that replaces Python's full-screen Textual app. [ADR 0001](adr/0001-inline-terminal-interface.md) records the decision behind it. In short:

- The interface renders **inline**, like Claude Code: finished output goes into the terminal's own scrollback, and only a live region at the bottom is redrawn.
- It keeps **Python's behavior**: commands, messages, approvals, files, and cancellation.
- It covers **every screen** of Python's app, in five slices (section 10).

The Python sources this is measured against are under `src/agent86/` in the reference repository:

- `tui/app.py`, `tui/commands.py`, `tui/turn_bridge.py`, and `tui/mentions.py`
- `tui/widgets/*` and `tui/screens/*`
- `ui/status.py` and `ui/repl.py`
- `config_writer.py`, `cognitive/catalog.py`, and `cognitive/capabilities.py`

## 1. Goals and non-goals

Goals:

- **Look and feel:** a terminal interface that looks and behaves like Claude Code. That means a transcript in native scrollback, a bordered multi-line input box, `/` and `@` completion menus, a spinner while the agent works, inline approval panels, and a live status line.
- **Behavior:** Python's interactive behavior, observable in the same places: the same commands and messages, the same approval rules, the same session, history, and config files, and the same cancellation outcomes.
- **Coverage:** every capability of Python's full-screen app, including model, provider, key, and MCP management.
- **Platforms:** Windows Terminal, the VS Code terminal, Windows conhost, and macOS and Linux terminals.

Non-goals:

- Pixel-matching Textual. Python's layout, colours, and widget chrome are not a contract.
- Mouse support. Python's app is keyboard-driven too: toggling and picking are bound to keys.
- Features Python doesn't have, such as "don't ask again" approvals, themes, or vim mode.
- Hosting the interface in Git Bash's mintty without winpty. That case falls back to the plain loop (section 9).

## 2. What is a contract and what is presentation

| Area | Contract (must match Python) | Presentation (ours) |
|---|---|---|
| Commands | Registry, names, order, arguments, every printed message, `needs_choice` behavior (a bare `/model`, `/mode`, `/resume`, `/config model`, `/config mcp` opens a picker) | How the result is drawn |
| Turns | Text the model streams, harness notices, `[cancelled]`, error type and message, the hint `see agent86 trace show -s <id>`, the turn summary line | Markers, colours, layout |
| Tools | What a block shows: the name, the argument preview, the result's first line, and the full arguments and result when expanded; `ok` versus error colouring | `⏺`/`⎿` shape; expanded view lives in the transcript view |
| Approvals | When a prompt appears, what it shows (tool name, argument summary, tool detail such as the diff), the answers (approve, deny, Escape denies), deny on shutdown | Inline panel with numbered choices |
| Status line | Segments, their text, and the order they are shed under width pressure (`status_segments`, `fit_status_line`) | Where it sits and its colours |
| Files | `memory.db`, the history file, config write-back (comments preserved, secret guard, user/project scope), keyring entries | — |
| Startup | Surface selection rules, startup notes, MCP notes, the compact banner's content (version and model) | Banner drawn in Claude Code's style |
| Cancellation | Safe points, saved state, `turn_end` status `cancelled` | Key hints in the spinner line |

Every change to a contract row is a bug. A change to a presentation row is allowed, but it is recorded as a known difference when Python's tests would notice it.

## 3. Layout

The terminal has two parts. **Scrollback** is everything above the live region: the terminal's normal history, written once and never redrawn. The **live region** is at the bottom, redrawn on every change. It is drawn the way Ink, which Claude Code is built on, draws (`src/ui/screen.rs`):

- On each frame the renderer moves back to the top of the region and clears down.
- It prints any newly finished lines, which scroll up into history like ordinary output.
- Then it prints the region again.

The region's height varies with what it holds: it grows for menus and panels and shrinks back afterwards. Every live row is wrapped by us to one cell less than the terminal width, so the row count is always exact: some consoles wrap a row that reaches the last column at once.

```
╭─ agent86 ──────────────────────────╮        ┐
│ version  0.1.0                     │        │
│ model    openrouter:openai/gpt-4o  │        │
╰────────────────────────────────────╯        │
memory: …   (startup notes, dim)              │  scrollback
                                              │  (native, append-only)
> list the files and fix the typo in README   │
                                              │
⏺ list_dir(".")                               │
  ⎿ 12 entries                                │
                                              │
⏺ I found the typo on line 3.                 ┘
✻ Running edit_file… (14s · esc to interrupt)   ┐
╭────────────────────────────────────────────╮  │
│ > _                                        │  │  live region
╰────────────────────────────────────────────╯  │  (redrawn)
  gpt-4o · running edit_file… · sbx subprocess · mode: ask  [Shift+Tab]  ┘
```

From top to bottom, the live region holds:

1. **The streaming tail:** the part of the current reply that is not yet complete enough to commit (section 6). It is capped at about 40% of the screen height, like Python's `#stream`.
2. **The spinner line,** only while a turn runs. It shows a spinner glyph, the phase (`Thinking…`, `Running edit_file…`, `Cancelling…`), and the elapsed time. The hint is `esc to interrupt`. With `[ui] spinner = false` the glyph is static.
3. **A menu or a panel.** A menu (the `/` commands or `@` paths) sits under the input box. A panel (an approval or a picker) replaces the input box while it is open.
4. **The input box:** bordered, one to eight rows, scrolling after eight, like Python's `PromptInput`. It is disabled while a turn runs, but it still shows its draft.
5. **The status line:** Python's `fit_status_line` text, dim, one row.

## 4. Visual vocabulary

| Entry | Python's full-screen app | Here |
|---|---|---|
| Banner | Compact Rich panel: `version`, `model` | Same content, rounded cyan border |
| User prompt | `> text` (bold) | `> text` (bold, grey background band, like Claude Code) |
| Reply | `agent86 ` label (bold cyan), then text; Markdown when finished | `⏺ ` then the text, Markdown rendered per block; continuation lines indented two spaces |
| Tool call | `▸ name({args…}) → summary` (dim; red when the call failed) | `⏺ name(args…)` (bold name) and on the next line `  ⎿ summary` (dim; red when the call failed); the collapsed lengths follow Python (`ARG_PREVIEW_LIMIT` 72, `SUMMARY_LIMIT` 100) |
| Harness notice | dim line | dim line |
| MCP note | `mcp: …` yellow | same |
| Mention refusal | dim yellow | same |
| Error | `error: Type: message` (red) and the hint (dim) | same text, `⏺` marker in red |
| Cancelled | `[cancelled]` notice, `cancelling…` note | same text |
| Turn summary | dim line | dim line |

Text from users, models, tools, MCP servers, and file names is never interpreted as markup or as terminal control sequences. Control characters are stripped or replaced before drawing, the same care Python takes with `Text(...)` in place of markup.

## 5. Keys

| Key | Python's full-screen app | Here |
|---|---|---|
| Enter | Submit (or pick from the open menu) | Same |
| Shift+Enter, Ctrl+J | Newline | Same; Shift+Enter needs a terminal that reports it (crossterm keyboard enhancement), and Ctrl+J always works |
| Up / Down | History, only from the first or last line; menu navigation when a menu is open | Same |
| Tab | Moves focus away from the prompt | Accept the highlighted menu item; on a directory, accept and keep completing (Python's "a second Tab descends") |
| Escape | Close the menu; else cancel a running turn; else clear the draft | Same order |
| Ctrl+C | Cancel a running turn; when idle or already cancelling, quit | Cancel a running turn. When idle, the first press clears a draft, and on an empty prompt it shows `Press Ctrl+C again to exit` for two seconds, as Claude Code does. A second press quits. **Known difference** |
| Ctrl+D | — (Textual) | On an empty prompt, quit (the plain loop's end-of-input) |
| Shift+Tab | Cycle the approval mode (ask, auto, deny) | Same |
| Ctrl+O | Expand or collapse the last tool block in place | Open the transcript view (section 7). **Known difference** |
| Ctrl+Shift+O | Expand or collapse all tool blocks | In the transcript view, toggle all blocks; it is not reported on every terminal, so `a` does the same there |
| In panels | `y`/`n`/Escape on approval; arrow keys and Enter on pickers | Same keys, plus number keys for numbered choices |

## 6. Streaming and Markdown

The text the harness streams is accumulated exactly, as Python's `ReplyEntry.text` is. Committing works like this:

- **During a reply:**
  - A block is committed to scrollback once it is complete: a paragraph that ends in a blank line outside a code fence, a whole fenced code block, a list, a table, or a heading line.
  - With `[ui] markdown` on, a committed block is rendered as Markdown (`pulldown-cmark`). With it off, the block is plain text.
  - The unfinished tail stays plain in the live region.
  - A tail longer than 2,000 characters without a block boundary is committed at the last newline, or hard-split if there is none. This is Python's `_STREAM_TAIL_LIMIT` rule.
- **At a tool call, a notice, or the end of the turn:** the tail is flushed. It is rendered as Markdown if it parses as a complete block.
- **Under `egress = "redact"`:** the loop already buffers each step, so nothing unredacted reaches the interface.

Python renders the whole reply as one Markdown document once it is finished. Rendering block by block gives the same result for every block that can't span a blank line. Lists and tables are held until they end, so loose lists also render the same.

Code blocks are shown with a dim border and no syntax highlighting at first. Python highlights them with Pygments (`monokai`). Whether to add `syntect` is decided in slice 4 against binary size; until then it is a known difference.

## 7. Transcript view (Ctrl+O)

Scrollback can't be edited, so the expanded tool blocks Python shows in place are shown in a full-screen view instead:

- **Opening:** Ctrl+O enters the alternate screen and shows the whole session transcript from the interface's entry model (section 8). Each tool block is expanded the way Python's `ToolBlockEntry.body_lines` is: the arguments as pretty JSON, then the result, capped at `RESULT_LINE_LIMIT`, 200 lines.
- **Keys:** arrows, PgUp/PgDn, Home, and End scroll. Ctrl+Shift+O or `a` toggles all blocks between expanded and collapsed. Escape, `q`, or Ctrl+O returns.
- **Returning** restores the screen exactly as it was, because the alternate screen leaves scrollback untouched.
- **During a turn:** the view can be opened while a turn runs. The turn keeps going, and new entries appear in the view.

## 8. Architecture

### Modules

| Path | Holds |
|---|---|
| `src/ui/mod.rs` | `run_interactive`: surface selection, then the full interface or the plain loop on the same `Repl`. It also holds the terminal guard (raw mode, bracketed paste, keyboard enhancement, Windows console checks, and a panic hook that restores the terminal), the event pump, and the turn worker |
| `src/ui/app.rs` | `App`, the state machine: what has been committed, the reply tail, running tools, the approval panel, and what each key and `UiEvent` does. It never touches the terminal, so tests drive it directly |
| `src/ui/screen.rs` | The inline renderer: frames of committed lines and the live region, the caret, and row accounting across resizes |
| `src/ui/text.rs` | Styled `Line`s, wrapping in terminal cells, truncation, and `sanitize` for untrusted text |
| `src/ui/entries.rs` (part 3) | The transcript entry model: `User`, `Reply`, `Tool`, `Notice`, `Error`, `Raw`. It is kept for the transcript view and for rebuilding a resumed session (Python's `_entries_for`) |
| `src/ui/render/` (part 4) | `markdown.rs` and `diff.rs`; the full-screen transcript view may use ratatui |
| `src/ui/input.rs` | The multi-line editor: cursor, editing keys, word movement, paste, and history navigation over `PromptHistory` |
| `src/ui/menu.rs` | The `/` command menu and `@` path completion (a port of `complete_mentions`) |
| `src/ui/status.rs` | A port of `ui/status.py` and `fit_status_line`. Shared, so `/cost` and the plain loop could use the same numbers |
| `src/ui/panels/*.rs` | One file per Python screen: `approval`, `mode_picker`, `model_picker`, `session_picker`, `provider_manager` (with the catalog picker), `key_entry`, `connection_test`, `save_diff`, `mcp_manager` (with the server form), `mcp_test` |
| `src/repl.rs` | Unchanged role: the command registry, `Repl`, and the plain loop. Command handlers already return `CommandResult`s, which both surfaces draw |

### Threads and events

- **The UI thread** owns the terminal. It reads crossterm events, receives `UiEvent`s over one channel, redraws the live region, and commits finished lines.
- **A turn runs on a worker thread** with its own tokio runtime, as the plain loop's turn does now. The worker owns the `Harness` and the `AgentState` for the duration of the turn and hands both back when it finishes. Nothing is shared under a lock while the UI draws. The UI keeps its own copies of what it displays (the status numbers and the entries).
- **The harness gains a turn observer.** It is an optional sink of structured `TurnEvent`s: `Text`, `ToolStart { call }`, `ToolResult { call_id, result }`, `Notice`, `Phase`, and `Finished`. These are emitted alongside the existing text `emit`, so `run`, `run --json`, and the plain loop print exactly what they print today. Python's `turn_bridge` parses `[tool] …` lines and looks the arguments up in the state. Here the events carry the call and the result directly.
- **Approvals:** the gate's prompt closure, running on the worker, sends an `ApprovalRequest` with a one-shot reply channel and blocks on it. The UI opens the approval panel and answers. On shutdown, every pending request is answered with deny, as Python's `_shutdown_workers` does.
- **Cancellation:** cancellation uses the existing `CancelToken`. Escape and Ctrl+C call `cancel()` and write the `cancelling…` note, and the turn ends at its next safe point.
- **MCP notes:** these arrive through `McpManager::add_note_listener` and are forwarded as `UiEvent`s, so they show up even while idle (Python's `on_mcp_note`).
- **Slow work never blocks the UI thread.** That covers catalog fetches, connection tests, and MCP tests, which run on workers and report back with events (Python's `CatalogReady` and the test modals' workers).

### Surface selection

Python's `_use_tui` decides, then the start is attempted:

1. If `--plain` is given or `AGENT86_PLAIN` is set, use the plain loop.
2. If `[ui] tui` is false, or stdin or stdout is not a terminal, use the plain loop.
3. Otherwise, build the `Repl` once (as Python's `run_repl` does), then try to enable raw mode and create the inline viewport. On failure, print `TUI could not start (<reason>); using plain REPL.` (dim) and run the plain loop on the same `Repl`. This catches mintty-style pipes that `is_terminal` reports as terminals.

## 9. Platforms

- **Windows Terminal, VS Code, and conhost:** supported through crossterm's console APIs. Shift+Enter is reported only where keyboard enhancement is available, and Ctrl+J is always there.
- **macOS and Linux terminals:** supported. Keyboard enhancement (the kitty protocol) is enabled where the terminal supports it.
- **Git Bash's mintty without winpty:** falls back to the plain loop with the note. `winpty agent86` or Windows Terminal gives the full interface.
- **Resize:** the live region is redrawn at the new width. Scrollback is the terminal's, and it reflows it or not.
- **Wide characters and emoji:** measured with `unicode-width`.

## 10. Slices

Each slice is one PR to `main`. Each passes `cargo test --all-targets`, `cargo fmt`, and clippy with `-D warnings`. Each updates `docs/PROJECT_PLAN.md`, the README, and CLAUDE.md, and is checked by hand at a real terminal before it merges.

### Slice 1 (stage 9, part 2): the inline base

- **Surface and terminal:**
  - Surface selection and the fallback note.
  - Terminal setup and restore, including after a panic.
- **Turn display:**
  - The compact banner and startup notes in scrollback.
  - User prompt echo.
  - Streamed replies as plain text.
  - Tool calls as `⏺`/`⎿` blocks, through the new turn observer.
  - Notices, MCP notes, errors with the hint, and the turn summary.
- **Live region:**
  - The input box with multi-line editing and shared history.
  - The spinner line and the status line, which needs a port of `ui/status.py` and the context window lookup (`[model.context_window]` overrides, the substring table, and the default). The OpenRouter context-length lookup comes in slice 4.
- **Interaction:**
  - Approvals as an inline panel (approval mode `ask` is the default, so this can't wait).
  - Slash commands typed in full, their output drawn in scrollback.
  - `/clear`, which clears the screen and reprints the banner.
  - Escape and Ctrl+C cancellation, Ctrl+C twice or Ctrl+D to quit, and Shift+Tab.

### Slice 2 (stage 9, part 3): menus and pickers

- **Menus:**
  - The `/` command menu (Python's rule: the line starts with `/` and has no space).
  - `@` path completion (`complete_mentions`: 20 results, directories first with `/`, quoted when a path has a space).
- **Pickers that bare `needs_choice` commands open:**
  - The mode picker.
  - The model picker, with the configured role slots (`model.default`, `model.route.cheap`, `model.route.frontier`).
  - The session picker, with type-to-filter and Python's labels.
- **Resume:**
  - Resuming rebuilds the transcript: the screen is cleared, the banner is printed, then the session's entries and the `resumed session <id> (<n> messages) - <title>` note.

### Slice 3 (stage 9, part 4): rich rendering

- Markdown rendering per block (`[ui] markdown`).
- Coloured diffs in the approval panel and the transcript view.
- The Ctrl+O transcript view.
- The decision on syntax highlighting.

### Slice 4 (stage 9, part 5): models, providers, and keys

- **Model catalogs:**
  - A port of `cognitive/catalog.py` (the OpenAI-compatible `/v1/models` shape; providers that aren't ported report the catalog as unavailable). It is cached per session and fetched on a worker.
  - Catalog entries in the model picker.
  - The typed `/model <bare id>` fallback, which resolves against the active provider's catalog, with Python's queueing rules.
  - The OpenRouter context-length lookup, cached for 24 hours under `~/.agent86/cache`.
- **Config write-back:**
  - Comment-preserving write-back (`toml_edit`), a port of `config_writer.py`: `plan_edit` and `apply_edit`, user and project scopes, the secret leaf-key guard (only `${VAR}` references allowed), atomic writes, and a reload.
- **`/config model` chain:**
  - The provider manager, then key entry (masked, held in memory until the test passes), the catalog picker, the connection test (a real one-token completion with a 15-second cap and "Save anyway"), and the save-diff panel.
  - Storing the key in the keyring only after the test passes.
- **Command wording:** `/config model` in the plain loop changes to Python's wording.

### Slice 5 (stage 9, part 6): MCP management

- **Runtime server control:** `McpManager` gains the ability to start, stop, and remove a server while the app runs, with tools registered and unregistered (Python's `add_mcp_server`, `remove_mcp_server`, and `stop_server`).
- **`/config mcp` chain:**
  - The MCP manager: list, add from JSON or by hand, edit, remove, and enable or disable.
  - The server form, with validation messages taken from the config model.
  - `${VAR}` prompting through the masked entry.
  - The MCP test: a live start with a 30-second cap that lists the tools, with "Save anyway".
  - The save-diff panel, then mounting or unmounting the server live.
  - A cancelled add stops the server that its test started.
- **Command wording:** `/config mcp` in the plain loop changes to Python's wording.

Stage 9 is complete after slice 5, and then the README roadmap item is ticked.

## 11. Testing

- **Pure units:** status segments and shedding (checked against Python's numbers), the context window lookup, `/` menu filtering, `complete_mentions`, editor operations and history navigation, tool block text and truncation, Markdown block splitting, and the commit rule.
- **Rendering:** the live region and committed lines are rendered at fixed widths and compared as text, as is the renderer's output (its cursor movement and clearing). Styles are checked only where the style is the contract, such as an error or a failed tool being red.
- **App:** the `App` reducer is driven with synthetic key events and `UiEvent`s, with no real terminal. These tests cover a full turn, an approval answered yes and no, cancellation by Escape and Ctrl+C, shutdown with a pending approval, `/clear`, `/resume`, and the menus.
- **Harness:** tests check that the turn observer's events match the text `emit` (the same calls, results, and order, including in parallel batches) and that `run`'s output is unchanged.
- **Plain loop:** `tests/repl.rs` keeps running the plain loop through piped stdin. A unit test checks the selection rule (`--plain`, `AGENT86_PLAIN`, `[ui] tui = false`, no terminal).
- **Manual checklist per slice:** Windows Terminal, the VS Code terminal, and conhost on Windows. Also mintty, where the fallback note should appear, and macOS or Linux where available. Check a live turn with a tool call, an approval, a cancel, a resize, a long answer, and quitting with the terminal restored.

## 12. Risks

| Risk | Mitigation |
|---|---|
| A crash or panic leaves the terminal in raw mode | A panic hook and a drop guard restore it; tested by forcing a panic in a test build |
| Output from elsewhere writes to stdout and corrupts the live region | Everything goes through the UI channel; tools and MCP servers already capture their child output; stray `eprintln!` in interactive paths is removed |
| Flicker while streaming | Redraws only on change, at most about 30 per second; the streaming tail is bounded |
| Windows key reporting differs (no Shift+Enter, key-release events) | Act on key presses only; Ctrl+J for a newline; tested in conhost |
| Block-by-block Markdown renders a list or table differently from Python's whole-document render | Hold lists and tables until they end; compare against Rich's output for a set of sample replies |
| The interface grows too large for one PR | The five slices above, each usable on its own |
