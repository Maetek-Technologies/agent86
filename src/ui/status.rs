//! The status line, ported from Python's `ui/status.py` (what it says) and
//! `tui/widgets/status_footer.py` (how much of it fits).
//!
//! Under width pressure the line sheds, in order, the `[Shift+Tab]` hint (also in
//! `/help`), the token counts (one `/cost` away), the working directory's parents
//! (its last component stays), and the context gauge (the last to go, since a full
//! window is the failure it exists to show). The model, the directory, the cost,
//! the approval mode, and the working indicator are never shed: they say what is
//! running, where, what it costs, and whether it can act without asking.
//!
//! The working directory is beyond Python, whose status line has none: the banner
//! that names it scrolls away, and the status line is the one row always on screen.

use std::path::Path;

use crate::pricing::format_cost;
use crate::tools::display_path;
use crate::turn::human_tokens;
use unicode_width::UnicodeWidthStr;

/// Segment keys, in the order they are given up. `cwd` is shortened, not dropped.
const SHED_ORDER: [&str; 4] = ["hint", "tok", "cwd", "ctx"];

/// What the status line reports, as Python's `StatusState`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusState {
    /// The bare model id.
    pub model: String,
    /// The working directory as shown (see `short_path`); empty shows none.
    pub cwd: String,
    /// Input tokens of the latest model call: how full the context is.
    pub used_tokens: u64,
    pub window: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// Whether the model has a known price, so the cost is a dollar figure.
    pub priced: bool,
    pub sandbox: String,
    pub approval: String,
    /// A turn is in flight.
    pub working: bool,
    /// What the turn is doing: `thinking`, `running write_file`, `cancelling`.
    pub phase: String,
    pub hotkey_hint: String,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// Percent of the context window filled, clamped to 0..=100.
pub fn context_percent(used: u64, window: u64) -> u64 {
    if window == 0 {
        return 0;
    }
    // Python's round() rounds halves to even.
    let percent = (100.0 * used as f64 / window as f64).round_ties_even();
    percent.clamp(0.0, 100.0) as u64
}

/// `path` for display: no `\\?\` prefix, and the home directory written `~`.
pub fn short_path(path: &Path, home: Option<&Path>) -> String {
    let shown = display_path(path);
    let Some(home) = home else {
        return shown;
    };
    let home = display_path(home);
    let home = home.trim_end_matches(['/', '\\']);
    let same = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    if home.is_empty() || shown.len() < home.len() || !shown.is_char_boundary(home.len()) {
        return shown;
    }
    let (head, rest) = shown.split_at(home.len());
    if !same(head, home) {
        return shown;
    }
    if rest.is_empty() {
        "~".to_owned()
    } else if rest.starts_with(['/', '\\']) {
        format!("~{rest}")
    } else {
        shown
    }
}

/// The last component of a shown path (`~/src/app` → `app`).
fn last_component(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

/// The line's labelled parts, in display order, as Python's `status_segments`,
/// plus the working directory after the model.
pub fn status_segments(state: &StatusState) -> Vec<(&'static str, String)> {
    let mut segments = vec![("model", state.model.clone())];
    if !state.cwd.is_empty() {
        segments.push(("cwd", state.cwd.clone()));
    }
    if state.working {
        let phase = if state.phase.is_empty() {
            "working"
        } else {
            &state.phase
        };
        segments.push(("phase", format!("{phase}…")));
    } else {
        let percent = context_percent(state.used_tokens, state.window);
        segments.push((
            "ctx",
            format!(
                "ctx {percent}% ({}/{})",
                human_tokens(state.used_tokens),
                human_tokens(state.window)
            ),
        ));
        let mut tokens = format!(
            "tok {}/{}",
            human_tokens(state.used_tokens),
            human_tokens(state.output_tokens)
        );
        let cached = state.cache_read_tokens + state.cache_creation_tokens;
        if cached > 0 {
            tokens.push_str(&format!(" ({} cached)", human_tokens(cached)));
        }
        segments.push(("tok", tokens));
        segments.push(("cost", format_cost(state.cost_usd, state.priced)));
    }
    segments.push(("sbx", format!("sbx {}", state.sandbox)));
    segments.push(("mode", format!("mode: {}", state.approval)));
    if !state.hotkey_hint.is_empty() {
        segments.push(("hint", format!("[{}]", state.hotkey_hint)));
    }
    segments
}

/// Segments joined with ` · `, the key hint set off at the end.
pub fn join_segments(segments: &[(&str, String)]) -> String {
    let body: Vec<&str> = segments
        .iter()
        .filter(|(key, _)| *key != "hint")
        .map(|(_, text)| text.as_str())
        .collect();
    let body = body.join(" · ");
    match segments.iter().find(|(key, _)| *key == "hint") {
        Some((_, hint)) => format!("{body}  {hint}"),
        None => body,
    }
}

/// The line, no wider than `width` cells when shedding can manage it (`0` means
/// unlimited), as Python's `fit_status_line`. The protected segments are never cut.
pub fn fit_status_line(state: &StatusState, width: usize) -> String {
    let mut segments = status_segments(state);
    let mut line = join_segments(&segments);
    if width == 0 || line.width() <= width {
        return line;
    }
    for key in SHED_ORDER {
        if key == "cwd" {
            for (segment, text) in segments.iter_mut() {
                if *segment == "cwd" {
                    *text = last_component(text).to_owned();
                }
            }
        } else {
            segments.retain(|(segment, _)| *segment != key);
        }
        line = join_segments(&segments);
        if line.width() <= width {
            break;
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state() -> StatusState {
        StatusState {
            model: "gpt-4o-mini".into(),
            cwd: String::new(),
            used_tokens: 3_400,
            window: 128_000,
            output_tokens: 612,
            cost_usd: 0.0123,
            priced: true,
            sandbox: "subprocess".into(),
            approval: "ask".into(),
            working: false,
            phase: String::new(),
            hotkey_hint: "Shift+Tab".into(),
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    #[test]
    fn the_idle_line_matches_python() {
        assert_eq!(
            fit_status_line(&state(), 0),
            "gpt-4o-mini · ctx 3% (3.4k/128k) · tok 3.4k/612 · $0.0123 · sbx subprocess \
             · mode: ask  [Shift+Tab]"
        );
    }

    #[test]
    fn a_working_line_shows_the_phase_instead_of_the_numbers() {
        let mut working = state();
        working.working = true;
        working.phase = "running write_file".into();
        assert_eq!(
            fit_status_line(&working, 0),
            "gpt-4o-mini · running write_file… · sbx subprocess · mode: ask  [Shift+Tab]"
        );
        working.phase.clear();
        assert!(fit_status_line(&working, 0).contains(" · working… · "));
    }

    #[test]
    fn narrow_lines_shed_the_hint_then_tokens_then_the_gauge() {
        let full = fit_status_line(&state(), 0);
        let no_hint = fit_status_line(&state(), full.width() - 1);
        assert!(!no_hint.contains("Shift+Tab"));
        assert!(no_hint.contains("tok "));
        let no_tokens = fit_status_line(&state(), no_hint.width() - 1);
        assert!(!no_tokens.contains("tok ") && no_tokens.contains("ctx "));
        let minimal = fit_status_line(&state(), 10);
        assert_eq!(
            minimal,
            "gpt-4o-mini · $0.0123 · sbx subprocess · mode: ask"
        );
    }

    #[test]
    fn cached_tokens_and_unpriced_models_are_reported() {
        let mut cached = state();
        cached.cache_read_tokens = 1_500;
        cached.cache_creation_tokens = 400;
        cached.priced = false;
        let line = fit_status_line(&cached, 0);
        assert!(line.contains("tok 3.4k/612 (1.9k cached)"), "{line}");
        assert!(line.contains("cost n/a (unpriced model)"), "{line}");
    }

    #[test]
    fn the_working_directory_follows_the_model_and_is_shortened_not_shed() {
        let mut here = state();
        here.cwd = "~/src/app".into();
        let full = fit_status_line(&here, 0);
        assert!(full.starts_with("gpt-4o-mini · ~/src/app · ctx "), "{full}");
        let no_tokens = fit_status_line(&here, full.width() - 20);
        assert!(no_tokens.contains(" · ~/src/app · "), "{no_tokens}");
        let minimal = fit_status_line(&here, 10);
        assert_eq!(
            minimal,
            "gpt-4o-mini · app · $0.0123 · sbx subprocess · mode: ask"
        );
    }

    #[test]
    fn home_is_written_as_a_tilde() {
        let sep = std::path::MAIN_SEPARATOR;
        let home = Path::new(if cfg!(windows) {
            r"C:\Users\me"
        } else {
            "/home/me"
        });
        assert_eq!(short_path(home, Some(home)), "~");
        assert_eq!(
            short_path(&home.join("src").join("app"), Some(home)),
            format!("~{sep}src{sep}app")
        );
        let sibling = PathBuf::from(format!("{}2", home.display()));
        assert_eq!(
            short_path(&sibling, Some(home)),
            sibling.display().to_string()
        );
        assert_eq!(short_path(home, None), home.display().to_string());
        assert_eq!(last_component(&format!("~{sep}src{sep}app")), "app");
        assert_eq!(last_component("~"), "~");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_paths_and_case_are_handled_on_windows() {
        let home = Path::new(r"C:\Users\Me");
        assert_eq!(
            short_path(Path::new(r"\\?\c:\users\me\work"), Some(home)),
            r"~\work"
        );
    }

    #[test]
    fn the_gauge_is_clamped() {
        assert_eq!(context_percent(0, 0), 0);
        assert_eq!(context_percent(500, 1000), 50);
        assert_eq!(context_percent(5000, 1000), 100);
    }
}
