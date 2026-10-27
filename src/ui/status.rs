//! The status line, ported from Python's `ui/status.py` (what it says) and
//! `tui/widgets/status_footer.py` (how much of it fits).
//!
//! Under width pressure the line sheds, in order, the `[Shift+Tab]` hint (also in
//! `/help`), the token counts (one `/cost` away), and the context gauge (the last to
//! go, since a full window is the failure it exists to show). The model, the cost,
//! the approval mode, and the working indicator are never shed: they say what is
//! running, what it costs, and whether it can act without asking.

use crate::pricing::format_cost;
use crate::turn::human_tokens;
use unicode_width::UnicodeWidthStr;

/// Segment keys, in the order they are given up.
const SHED_ORDER: [&str; 3] = ["hint", "tok", "ctx"];

/// What the status line reports, as Python's `StatusState`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusState {
    /// The bare model id.
    pub model: String,
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

/// The line's labelled parts, in display order, as Python's `status_segments`.
pub fn status_segments(state: &StatusState) -> Vec<(&'static str, String)> {
    let mut segments = vec![("model", state.model.clone())];
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
        segments.retain(|(segment, _)| *segment != key);
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

    fn state() -> StatusState {
        StatusState {
            model: "gpt-4o-mini".into(),
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
    fn the_gauge_is_clamped() {
        assert_eq!(context_percent(0, 0), 0);
        assert_eq!(context_percent(500, 1000), 50);
        assert_eq!(context_percent(5000, 1000), 100);
    }
}
