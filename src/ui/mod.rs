//! The interactive interface: Python's full-screen app reworked to render inline,
//! in Claude Code's style ([ADR 0001](../../docs/adr/0001-inline-terminal-interface.md)).
//!
//! The conversation is printed into the terminal's own scrollback; only a live
//! region at the bottom (the streaming reply, running tools, a spinner, the input
//! box or an approval panel, and the status line) is redrawn. The commands,
//! messages, approvals, files, and cancellation are Python's, shared with the plain
//! loop through [`Repl`].
//!
//! Surface selection follows Python's `run_repl`: the session is built once, then
//! this interface runs on it unless `--plain`, `AGENT86_PLAIN`, `[ui] tui = false`,
//! or a non-terminal stdin or stdout says otherwise. A terminal that cannot host it
//! (Git Bash's mintty without winpty) falls back to the plain loop with a note.

mod app;
mod banner;
mod diff;
mod entries;
mod input;
mod limits;
mod manage;
mod manage_mcp;
mod markdown;
mod mcp_config;
mod mcp_panels;
mod menu;
mod panels;
mod picker;
mod screen;
pub mod status;
mod text;

use crate::agent::Harness;
use crate::config::Config;
use crate::config::ConfigPaths;
use crate::mentions::expand_mentions;
use crate::provider::capabilities::context_window_with;
use crate::provider::remote_windows::{cache_path, discover_context_window};
use crate::repl::{
    find_command_for_line, now, plain_loop, session_label, start_repl, Action, Choice, Repl, Style,
    NO_MEMORY_NOTE,
};
use crate::secrets::SecretStore;
use app::{App, Effect, Panel, Request, UiEvent};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use manage::Manager;
use panels::{KeyEntry, KeyPurpose, SavePurpose};
use picker::{Picker, PickerKind};
use screen::Screen;
use status::{short_path, StatusState};
use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::sync::mpsc;
use std::time::Duration;

/// How long the event loop waits for a key before redrawing anyway.
const FRAME: Duration = Duration::from_millis(50);

/// `agent86` with no subcommand: this interface, or the plain loop.
///
/// `paths` is where the configuration was read from, for `/config model` to write
/// it back.
pub fn run_interactive(
    config: Config,
    paths: ConfigPaths,
    resume: Option<&str>,
    plain: bool,
    secrets: Box<dyn SecretStore>,
) {
    let wanted = wants_interface(&config, plain);
    let Some(mut repl) = start_repl(config, resume, secrets) else {
        return;
    };
    repl.config_paths = Some(paths);
    if !wanted {
        return plain_loop(repl);
    }
    let terminal = match Terminal::start() {
        Ok(terminal) => terminal,
        Err(error) => {
            let mut note = crate::repl::Text::styled(
                Style::Dim,
                format!("TUI could not start ({error}); using plain REPL."),
            );
            note.push(Style::Plain, "\n");
            print!("{}", note.render(color_allowed()));
            return plain_loop(repl);
        }
    };
    let outcome = run(&mut repl);
    drop(terminal);
    if let Err(error) = outcome {
        eprintln!("error: the interface stopped: {error}");
    }
}

/// Python's `_use_tui`: not when asked for the plain loop, and only at a terminal.
fn wants_interface(config: &Config, plain: bool) -> bool {
    interface_wanted(
        config.ui.tui,
        plain || std::env::var_os("AGENT86_PLAIN").is_some_and(|value| !value.is_empty()),
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    )
}

fn interface_wanted(tui: bool, plain: bool, terminal: bool) -> bool {
    tui && !plain && terminal
}

fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// Raw mode and the terminal features the interface uses, undone on drop (and on
/// a panic, so a crash never leaves the shell unusable).
struct Terminal {
    keyboard_enhanced: bool,
}

impl Terminal {
    fn start() -> io::Result<Self> {
        #[cfg(windows)]
        windows_console()?;
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        let _ = crossterm::execute!(stdout, event::EnableBracketedPaste);
        // Lets terminals that support it report Shift+Enter. Windows reports it anyway.
        let keyboard_enhanced = !cfg!(windows)
            && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
            && crossterm::execute!(
                stdout,
                event::PushKeyboardEnhancementFlags(
                    event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            )
            .is_ok();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal(keyboard_enhanced);
            previous(info);
        }));
        Ok(Self { keyboard_enhanced })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // The panic hook stays installed: restoring a terminal that is already
        // restored does nothing.
        restore_terminal(self.keyboard_enhanced);
    }
}

fn restore_terminal(keyboard_enhanced: bool) {
    let mut stdout = io::stdout();
    if keyboard_enhanced {
        let _ = crossterm::execute!(stdout, event::PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(
        stdout,
        event::DisableBracketedPaste,
        crossterm::cursor::Show
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// The interface reads keys from the console and draws with VT sequences. Under
/// mintty, stdin is a pipe that Rust reports as a terminal but no console reads
/// from, so that case is refused here and the plain loop is used instead.
#[cfg(windows)]
fn windows_console() -> io::Result<()> {
    use crossterm_winapi::{ConsoleMode, Handle};
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    ConsoleMode::from(Handle::input_handle()?)
        .mode()
        .map_err(|_| io::Error::other("stdin is not a console"))?;
    let output = ConsoleMode::from(Handle::output_handle()?);
    let mode = output
        .mode()
        .map_err(|_| io::Error::other("stdout is not a console"))?;
    output
        .set_mode(mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)
        .map_err(|_| io::Error::other("the console does not support VT sequences"))
}

/// What the status line shows for the session as it stands.
fn status_from(repl: &Repl) -> StatusState {
    let usage = &repl.state.usage;
    // OpenRouter is asked for its models' windows, cached beside the user config,
    // and Ollama for its model's maximum.
    let cache = repl
        .config_paths
        .as_ref()
        .and_then(|paths| paths.user.as_deref()?.parent().map(cache_path));
    let window = context_window_with(&repl.model_ref(), &repl.config, |provider, model| {
        discover_context_window(
            provider,
            model,
            &repl.config,
            cache.as_deref(),
            &*repl.secrets,
        )
    });
    StatusState {
        model: repl.harness.active_provider().model().to_owned(),
        cwd: short_path(
            repl.harness.context.policy.workspace(),
            crate::config::home_dir().as_deref(),
        ),
        used_tokens: repl
            .state
            .steps
            .last()
            .map_or(0, |step| step.usage.input_tokens),
        window,
        output_tokens: usage.output_tokens,
        cost_usd: usage.cost_usd,
        priced: repl.priced(),
        sandbox: repl.config.sandbox.mode.as_str().to_owned(),
        approval: repl.harness.gate.mode().as_str().to_owned(),
        working: false,
        phase: String::new(),
        hotkey_hint: "Shift+Tab".to_owned(),
        cache_read_tokens: usage.cache_read_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
    }
}

/// Print the startup header (Python prints its compact banner at these points).
fn commit_header(app: &mut App) {
    let header = banner::header(env!("CARGO_PKG_VERSION"), &app.status.cwd, app.width());
    app.commit_text(&header);
}

fn run(repl: &mut Repl) -> io::Result<()> {
    let (width, height) = crossterm::terminal::size()?;
    let mut screen = Screen::new(io::stdout(), color_allowed(), usize::from(width));
    let (events, inbox) = mpsc::channel::<UiEvent>();

    // Approvals become a panel. The turn's thread waits for the answer; if the
    // interface goes away first, the dropped sender reads as a decline.
    let approvals = events.clone();
    repl.harness.gate.prompt = Some(Box::new(move |tool, preview| {
        let (reply, answer) = mpsc::channel();
        let request = UiEvent::Approval {
            tool: tool.to_owned(),
            preview: preview.clone(),
            reply,
        };
        approvals.send(request).is_ok() && answer.recv().unwrap_or(false)
    }));
    // So does an offer to raise a limit the turn reached; a dropped sender stops.
    let offers = events.clone();
    repl.harness.session_limits.prompt = Some(Box::new(move |hit| {
        let (reply, answer) = mpsc::channel();
        let request = UiEvent::Limit {
            hit: hit.clone(),
            reply,
        };
        if offers.send(request).is_err() {
            return None;
        }
        answer.recv().ok().flatten()
    }));
    // And so does a secret the agent asks for; a dropped sender declines.
    let secrets = events.clone();
    repl.harness
        .context
        .secrets
        .set_prompt(Some(Box::new(move |request| {
            let (reply, answer) = mpsc::channel();
            let request = UiEvent::Secret {
                request: request.clone(),
                reply,
            };
            if secrets.send(request).is_err() {
                return None;
            }
            answer.recv().ok().flatten()
        })));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut manager = Manager::new(events.clone());
    let mut app = App::new(
        status_from(repl),
        repl.state.session_id.clone(),
        repl.config.ui.spinner,
    );
    app.set_size(usize::from(width), usize::from(height));
    app.workspace = Some(repl.harness.context.policy.workspace().to_path_buf());
    app.markdown = repl.config.ui.markdown;
    commit_header(&mut app);
    for note in &repl.startup_notes {
        app.note(Style::Dim, note);
    }
    let notices = events.clone();
    crate::update::start(repl.config.ui.update_check, move |text| {
        let _ = notices.send(UiEvent::Note(Style::Dim, text));
    });

    loop {
        let history = repl.history().entries().to_vec();
        let request = pump(
            &mut app,
            &mut screen,
            &inbox,
            &repl.harness,
            &mut repl.mcp_cursor,
            &history,
        )?;
        // A picker's answer is dispatched as the command it stands for, as
        // Python's `_on_mode_picked` and `_on_model_picked` do; it is not typed, so
        // it stays out of the history.
        let line = match request {
            Request::Exit => break,
            Request::TurnDone => continue,
            Request::Submit(text) => {
                let line = text.trim().to_owned();
                // A secret the user entered stays out of the history file.
                let kept = repl.harness.context.secrets.redact(&line);
                repl.history().append(&kept);
                Some(line)
            }
            Request::Picked(PickerKind::Mode, mode) => Some(format!("/mode {mode}")),
            Request::Picked(PickerKind::Model, reference) => Some(format!("/model {reference}")),
            Request::Picked(PickerKind::Session, session_id) => {
                resume_picked(&mut app, &mut screen, repl, &session_id)?;
                None
            }
            Request::Picked(PickerKind::Provider, name) => {
                manager.on_provider(&mut app, repl, &name);
                None
            }
            Request::Picked(PickerKind::Catalog, reference) => {
                manager.on_catalog_picked(&mut app, repl, &reference);
                None
            }
            Request::Key(KeyPurpose::Provider, key) => {
                manager.on_key_entered(&mut app, repl, key);
                None
            }
            Request::Key(KeyPurpose::McpVar, value) => {
                manager.mcp.on_var(&mut app, repl, value);
                None
            }
            // Secret entries answer as `Request::Secret`, which carries the name.
            Request::Key(KeyPurpose::Secret, _) => None,
            Request::Secret(name, value) => {
                match value {
                    Some(value) => {
                        repl.harness.context.secrets.set(&name, value);
                        app.note(
                            Style::Dim,
                            &format!(
                                "secret {name} set for this session · the agent sees only its name"
                            ),
                        );
                    }
                    None => app.note(Style::Dim, &format!("secret {name} not set")),
                }
                None
            }
            Request::Mcp(action) => {
                manager.mcp.on_action(&mut app, repl, action);
                None
            }
            Request::McpForm(draft) => {
                manager.mcp.on_form(&mut app, repl, *draft);
                None
            }
            Request::McpTested(outcome) => {
                manager.mcp.on_tested(&mut app, repl, outcome);
                None
            }
            // A passing test switches the model at once, as a dispatched line.
            Request::Tested(outcome) => manager.on_tested(&mut app, repl, outcome),
            Request::Saved(SavePurpose::Model, edit) => {
                Manager::on_saved(&mut app, repl, edit);
                None
            }
            Request::Saved(SavePurpose::McpServer, edit) => {
                manager.mcp.on_saved(&mut app, repl, edit);
                None
            }
            Request::Saved(SavePurpose::Limits, edit) => {
                limits::on_saved(&mut app, repl, edit);
                None
            }
            Request::Saved(SavePurpose::McpUnmount, edit) => {
                manager.mcp.on_unmount_saved(&mut app, repl, edit);
                None
            }
            Request::Note(style, text) => {
                app.note(style, &text);
                None
            }
            Request::Catalog {
                provider,
                result,
                purpose,
            } => {
                for full in manager.on_catalog(&mut app, repl, &provider, result, purpose) {
                    Manager::retry_model(&mut app, repl, &full);
                }
                None
            }
        };
        if let Some(line) = line {
            let carry_on = dispatch(
                &mut app,
                &mut screen,
                repl,
                &mut manager,
                &inbox,
                &events,
                &runtime,
                &line,
            )?;
            if !carry_on {
                break;
            }
        }
        app.status = status_from(repl);
        app.session_id = repl.state.session_id.clone();
        app.mark_dirty();
    }
    let (live, _) = app.live();
    screen.leave_alternate()?;
    screen.frame(&app.take_committed(), &live, None)?;
    screen.clear_live()
}

/// Run one line, as Python's `_dispatch_line`: echo it, open a picker for a bare
/// `needs_choice` command, else run the command or the turn. False means quit.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    app: &mut App,
    screen: &mut Screen<io::Stdout>,
    repl: &mut Repl,
    manager: &mut Manager,
    inbox: &mpsc::Receiver<UiEvent>,
    events: &mpsc::Sender<UiEvent>,
    runtime: &tokio::runtime::Runtime,
    line: &str,
) -> io::Result<bool> {
    app.echo_user(line);
    if let Some((entry, arg)) = find_command_for_line(line) {
        if let (Some(choice), true) = (entry.needs_choice, arg.is_empty()) {
            if open_choice(app, repl, manager, choice) {
                return Ok(true);
            }
        }
        // A typed model id may be resolved against the provider's catalog.
        if entry.name == "/model" && !arg.is_empty() {
            manager.dispatch_model(app, repl, &arg);
            return Ok(true);
        }
        // Saving shows the diff first, as `/config model` does.
        if entry.name == "/limits" && arg.trim() == "save" {
            limits::open_save(app, repl);
            return Ok(true);
        }
        // Checking and installing take a while, so they run off the drawing thread.
        if entry.name == "/update" {
            start_update(app, events, &arg);
            return Ok(true);
        }
        // A secret is typed into a masked panel, never the input box.
        if entry.name == "/secret" {
            if let Some(name) = crate::repl::secret_to_enter(&arg) {
                let replacing = repl.harness.context.secrets.contains(name);
                app.open_panel(Panel::Key(KeyEntry::secret(name, "", replacing)));
                return Ok(true);
            }
        }
    }
    let result = repl.handle_command(line);
    match result.action {
        Action::Exit => return Ok(false),
        Action::Noop => {}
        Action::Handled => {
            if result.clears_transcript {
                // The old exchange is gone from the session, so it leaves the screen too.
                app.clear_transcript();
                screen.clear_all()?;
                commit_header(app);
            } else if let Some(render) = &result.render {
                app.commit_text(render);
            }
        }
        Action::Turn => {
            let outcome = run_turn(app, screen, repl, inbox, events, runtime, line)?;
            return Ok(outcome != Request::Exit);
        }
    }
    Ok(true)
}

/// `/update` on a worker thread, its answer coming back as a note. The installer's
/// output is captured, since it would tear the live region; a failure shows its
/// last lines.
fn start_update(app: &mut App, events: &mpsc::Sender<UiEvent>, arg: &str) {
    let now = match arg.trim() {
        "" => false,
        "now" => true,
        _ => {
            app.note(Style::Plain, "usage: /update [now]");
            return;
        }
    };
    app.note(
        Style::Dim,
        if now {
            "updating agent86…"
        } else {
            "checking for a newer release…"
        },
    );
    let events = events.clone();
    std::thread::spawn(move || {
        let result = crate::update::plan().and_then(|plan| {
            if now {
                crate::update::apply(&plan, false)
            } else {
                Ok(plan.describe())
            }
        });
        let note = match result {
            Ok(text) => UiEvent::Note(Style::Plain, text),
            Err(error) => UiEvent::Note(Style::Red, error),
        };
        let _ = events.send(note);
    });
}

/// Open the picker a bare command asks for, as Python's `_run_or_chain`. False
/// when this surface has no picker for it yet, so the command's handler runs.
fn open_choice(app: &mut App, repl: &Repl, manager: &mut Manager, choice: Choice) -> bool {
    match choice {
        Choice::Mode => app.open_picker(Picker::mode(repl.harness.gate.mode().as_str())),
        // The picker opens once the active provider's catalog is in.
        Choice::Model => manager.model_picker(app, repl),
        Choice::ConfigModel => manager.open_provider_manager(app, repl),
        // A picker offering nothing is a dead end, so the empty cases are notes.
        Choice::Resume => match repl.recent_sessions(20) {
            None => app.note(Style::Dim, NO_MEMORY_NOTE),
            Some(sessions) if sessions.is_empty() => app.note(Style::Dim, "no saved sessions yet"),
            Some(sessions) => {
                let now = now();
                let choices = sessions
                    .iter()
                    .map(|info| (session_label(info, now), info.session_id.clone()))
                    .collect();
                app.open_picker(Picker::sessions(choices));
            }
        },
        Choice::ConfigMcp => manager.mcp.open(app, repl),
    }
    true
}

/// Make the picked session the live one and rebuild the screen from it, as
/// Python's `load_session`: the banner, the conversation, then the resumed note.
fn resume_picked(
    app: &mut App,
    screen: &mut Screen<io::Stdout>,
    repl: &mut Repl,
    session_id: &str,
) -> io::Result<()> {
    match repl.switch_session(session_id) {
        Err(message) => app.note(Style::Dim, &message),
        Ok(note) => {
            app.clear_transcript();
            screen.clear_all()?;
            commit_header(app);
            app.replay(&repl.state.messages);
            app.note(Style::Dim, &note);
        }
    }
    Ok(())
}

/// Run one turn on a worker thread while the interface keeps drawing and taking keys.
fn run_turn(
    app: &mut App,
    screen: &mut Screen<io::Stdout>,
    repl: &mut Repl,
    inbox: &mpsc::Receiver<UiEvent>,
    events: &mpsc::Sender<UiEvent>,
    runtime: &tokio::runtime::Runtime,
    line: &str,
) -> io::Result<Request> {
    // `@path` mentions are inlined for the model; refusals are shown and sent along.
    let mentions = expand_mentions(
        line,
        &repl.harness.context.policy,
        repl.config.tools.mention_max_bytes,
    );
    for problem in &mentions.errors {
        app.note(Style::DimYellow, problem);
    }
    app.start_turn();
    let outcome = std::thread::scope(|scope| {
        let Repl {
            harness,
            state,
            mcp_cursor,
            ..
        } = &mut *repl;
        let harness: &Harness = harness;
        let worker_events = events.clone();
        let prompt = mentions.prompt;
        scope.spawn(move || {
            let result = runtime.block_on(harness.run_turn_events(state, &prompt, line, |event| {
                let _ = worker_events.send(event.into());
            }));
            let error = result
                .err()
                .map(|error| (error.python_type().to_owned(), error.to_string()));
            let _ = worker_events.send(UiEvent::Finished(error));
        });
        let outcome = pump(app, screen, inbox, harness, mcp_cursor, &[]);
        if outcome.is_err() {
            // The screen failed mid-turn: stop the turn and decline anything it waits
            // on, so the worker can finish and the scope can end.
            harness.cancel.cancel();
            app.abandon();
            while let Ok(event) = inbox.recv() {
                if matches!(event, UiEvent::Finished(_)) {
                    break;
                }
            }
        }
        outcome
    });
    // Saved whether the turn finished, failed, or was cancelled.
    repl.persist(&repl.state);
    // Shown on the error and cancel paths too: a turn that failed halfway still spent tokens.
    if let Some(summary) = repl.turn_summary_line() {
        app.note(Style::Dim, &summary);
    }
    outcome
}

/// Draw and take input until the interface has a request: a line to dispatch, the
/// end of a turn, or the end of the session.
fn pump(
    app: &mut App,
    screen: &mut Screen<io::Stdout>,
    inbox: &mpsc::Receiver<UiEvent>,
    harness: &Harness,
    mcp_cursor: &mut u64,
    history: &[String],
) -> io::Result<Request> {
    loop {
        while let Ok(event) = inbox.try_recv() {
            app.on_turn_event(event);
        }
        // MCP notes arrive at any time, idle or not, as in Python's `on_mcp_note`.
        if let Some(mcp) = &harness.mcp {
            let (seq, notes) = mcp.notes_since(*mcp_cursor);
            *mcp_cursor = seq;
            for note in notes {
                app.note(Style::Yellow, &format!("mcp: {note}"));
            }
        }
        let mode = harness.gate.mode().as_str();
        if app.status.approval != mode {
            app.status.approval = mode.to_owned();
            app.mark_dirty();
        }
        let request = app.take_request();
        if app.view_open() {
            // The transcript view is on the alternate screen; what is committed
            // meanwhile waits and is printed into scrollback on the way back.
            screen.enter_alternate()?;
            if request.is_some() || app.needs_frame() {
                screen.page(&app.view_page())?;
            }
        } else {
            screen.leave_alternate()?;
            if request.is_some() || app.needs_frame() {
                let (live, caret) = app.live();
                screen.frame(&app.take_committed(), &live, caret)?;
            }
        }
        if let Some(request) = request {
            return Ok(request);
        }

        let mut queue = VecDeque::new();
        if event::poll(FRAME)? {
            queue.push_back(event::read()?);
        }
        while let Some(event) = queue.pop_front() {
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = paste_aware(key, &mut queue)?;
                    match app.on_key(key, history) {
                        Effect::None => {}
                        Effect::Cancel => harness.cancel.cancel(),
                        Effect::CycleMode => harness.gate.set_mode(harness.gate.mode().cycle()),
                    }
                }
                Event::Paste(text) => app.on_paste(&text),
                Event::Resize(width, height) => {
                    screen.resize(usize::from(width));
                    app.set_size(usize::from(width), usize::from(height));
                }
                _ => {}
            }
        }
        app.tick();
    }
}

/// Terminals without bracketed paste (Windows) deliver a paste as keystrokes, so a
/// pasted newline arrives as Enter. An Enter with more keys already waiting behind
/// it is part of a paste and becomes a newline instead of sending the draft.
fn paste_aware(key: KeyEvent, queue: &mut VecDeque<Event>) -> io::Result<KeyEvent> {
    if key.code != KeyCode::Enter || !key.modifiers.is_empty() {
        return Ok(key);
    }
    while event::poll(Duration::ZERO)? {
        queue.push_back(event::read()?);
    }
    let burst = queue
        .iter()
        .any(|event| matches!(event, Event::Key(next) if next.kind != KeyEventKind::Release));
    Ok(if burst {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)
    } else {
        key
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plain_loop_is_used_when_asked_for_or_without_a_terminal() {
        assert!(interface_wanted(true, false, true));
        assert!(!interface_wanted(false, false, true), "[ui] tui = false");
        assert!(
            !interface_wanted(true, true, true),
            "--plain or AGENT86_PLAIN"
        );
        assert!(
            !interface_wanted(true, false, false),
            "piped stdin or stdout"
        );
    }
}
