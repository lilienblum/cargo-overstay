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
        let dumb_terminal =
            std::env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
        Self {
            enabled: is_terminal && std::env::var_os("NO_COLOR").is_none() && !dumb_terminal,
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
}
