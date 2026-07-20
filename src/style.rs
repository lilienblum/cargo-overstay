//! Terminal-aware ANSI styling for human-facing CLI output.

use std::fmt::Display;
use std::io::IsTerminal;

const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy)]
pub(crate) struct Style {
    enabled: bool,
}

impl Style {
    pub(crate) fn stdout() -> Self {
        Self::for_terminal(std::io::stdout().is_terminal())
    }

    pub(crate) fn stderr() -> Self {
        Self::for_terminal(std::io::stderr().is_terminal())
    }

    fn for_terminal(is_terminal: bool) -> Self {
        let term = std::env::var("TERM").ok();
        Self {
            enabled: color_enabled(
                is_terminal,
                std::env::var_os("NO_COLOR").is_some(),
                term.as_deref(),
            ),
        }
    }

    pub(crate) fn heading(&self, value: impl Display) -> String {
        self.paint("1;4", value)
    }

    pub(crate) fn command(&self, value: impl Display) -> String {
        self.paint("1;36", value)
    }

    pub(crate) fn path(&self, value: impl Display) -> String {
        self.paint("4;36", value)
    }

    pub(crate) fn accent(&self, value: impl Display) -> String {
        self.paint("36", value)
    }

    pub(crate) fn success(&self, value: impl Display) -> String {
        self.paint("1;32", value)
    }

    pub(crate) fn warning(&self, value: impl Display) -> String {
        self.paint("1;33", value)
    }

    pub(crate) fn error(&self, value: impl Display) -> String {
        self.paint("1;31", value)
    }

    pub(crate) fn strong(&self, value: impl Display) -> String {
        self.paint("1", value)
    }

    pub(crate) fn muted(&self, value: impl Display) -> String {
        self.paint("2", value)
    }

    fn paint(&self, code: &str, value: impl Display) -> String {
        if self.enabled {
            format!("\x1b[{code}m{value}{RESET}")
        } else {
            value.to_string()
        }
    }
}

fn color_enabled(is_terminal: bool, no_color: bool, term: Option<&str>) -> bool {
    is_terminal && !no_color && !term.is_some_and(|value| value.eq_ignore_ascii_case("dumb"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_style_wraps_text_in_ansi_codes() {
        let style = Style { enabled: true };

        assert_eq!(
            style.heading("Tracked targets"),
            "\x1b[1;4mTracked targets\x1b[0m"
        );
        assert_eq!(style.path("/work/app"), "\x1b[4;36m/work/app\x1b[0m");
        assert_eq!(style.error("75 GiB"), "\x1b[1;31m75 GiB\x1b[0m");
    }

    #[test]
    fn disabled_style_leaves_text_unchanged() {
        let style = Style { enabled: false };

        assert_eq!(style.heading("Tracked targets"), "Tracked targets");
        assert_eq!(style.muted("2d ago"), "2d ago");
    }

    #[test]
    fn terminal_policy_enables_color_for_interactive_terminals() {
        assert!(color_enabled(true, false, Some("xterm-256color")));
    }

    #[test]
    fn terminal_policy_disables_color_for_redirected_output() {
        assert!(!color_enabled(false, false, Some("xterm-256color")));
    }

    #[test]
    fn terminal_policy_honors_no_color() {
        assert!(!color_enabled(true, true, Some("xterm-256color")));
    }

    #[test]
    fn terminal_policy_disables_color_for_dumb_terminals() {
        assert!(!color_enabled(true, false, Some("dumb")));
        assert!(!color_enabled(true, false, Some("DUMB")));
    }
}
