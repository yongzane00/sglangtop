//! DGXTOP-inspired color theme with NO_COLOR and 256-color fallbacks.
//!
//! Three modes:
//! - Truecolor (COLORTERM=truecolor/24bit): the intended dark blue/grey look.
//! - 256-color: nearest indexed colors.
//! - Monochrome (NO_COLOR set, or --no-color): no colors at all; emphasis
//!   comes only from bold/dim modifiers so every value stays readable.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Indexed256,
    Mono,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ColorMode,
    /// Page background (dark blue-grey).
    pub bg: Style,
    /// Section headings (cyan).
    pub heading: Style,
    /// Healthy / current values (bright green).
    pub value: Style,
    /// Secondary metrics (purple).
    pub secondary: Style,
    /// Plain text.
    pub text: Style,
    /// De-emphasized text (labels, units, separators).
    pub dim: Style,
    /// Warning (yellow).
    pub warn: Style,
    /// Failure / critical saturation (red).
    pub crit: Style,
    /// Unavailable values ("--" / N/A).
    pub na: Style,
    /// Selected row highlight.
    pub selected: Style,
    /// Footer key hints.
    pub key: Style,
    /// Tab bar: active tab.
    pub tab_active: Style,
    pub tab_inactive: Style,
}

impl Theme {
    pub fn detect(no_color_flag: bool, get_env: impl Fn(&str) -> Option<String>) -> Theme {
        // https://no-color.org/ — any non-empty value disables color.
        let no_color_env = get_env("NO_COLOR").is_some_and(|v| !v.is_empty());
        if no_color_flag || no_color_env {
            return Theme::mono();
        }
        let truecolor =
            get_env("COLORTERM").is_some_and(|v| v.contains("truecolor") || v.contains("24bit"));
        if truecolor {
            Theme::truecolor()
        } else {
            Theme::indexed()
        }
    }

    pub fn truecolor() -> Theme {
        let bg = Color::Rgb(16, 20, 31);
        Theme {
            mode: ColorMode::TrueColor,
            bg: Style::default().bg(bg),
            heading: Style::default()
                .fg(Color::Rgb(80, 200, 220))
                .add_modifier(Modifier::BOLD),
            value: Style::default().fg(Color::Rgb(90, 240, 120)),
            secondary: Style::default().fg(Color::Rgb(190, 130, 255)),
            text: Style::default().fg(Color::Rgb(200, 205, 215)),
            dim: Style::default().fg(Color::Rgb(110, 118, 135)),
            warn: Style::default().fg(Color::Rgb(240, 200, 60)),
            crit: Style::default()
                .fg(Color::Rgb(250, 90, 90))
                .add_modifier(Modifier::BOLD),
            na: Style::default().fg(Color::Rgb(90, 96, 110)),
            selected: Style::default()
                .bg(Color::Rgb(40, 50, 75))
                .add_modifier(Modifier::BOLD),
            key: Style::default()
                .fg(Color::Rgb(80, 200, 220))
                .add_modifier(Modifier::BOLD),
            tab_active: Style::default()
                .fg(Color::Rgb(16, 20, 31))
                .bg(Color::Rgb(80, 200, 220))
                .add_modifier(Modifier::BOLD),
            tab_inactive: Style::default().fg(Color::Rgb(110, 118, 135)),
        }
    }

    pub fn indexed() -> Theme {
        Theme {
            mode: ColorMode::Indexed256,
            bg: Style::default().bg(Color::Indexed(234)),
            heading: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            value: Style::default().fg(Color::LightGreen),
            secondary: Style::default().fg(Color::LightMagenta),
            text: Style::default().fg(Color::Gray),
            dim: Style::default().fg(Color::DarkGray),
            warn: Style::default().fg(Color::Yellow),
            crit: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            na: Style::default().fg(Color::DarkGray),
            selected: Style::default()
                .bg(Color::Indexed(24))
                .add_modifier(Modifier::BOLD),
            key: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            tab_active: Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            tab_inactive: Style::default().fg(Color::DarkGray),
        }
    }

    /// No colors whatsoever; only modifiers. Values must stay readable.
    pub fn mono() -> Theme {
        Theme {
            mode: ColorMode::Mono,
            bg: Style::default(),
            heading: Style::default().add_modifier(Modifier::BOLD),
            value: Style::default(),
            secondary: Style::default(),
            text: Style::default(),
            dim: Style::default().add_modifier(Modifier::DIM),
            warn: Style::default().add_modifier(Modifier::BOLD),
            crit: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            na: Style::default().add_modifier(Modifier::DIM),
            selected: Style::default().add_modifier(Modifier::REVERSED),
            key: Style::default().add_modifier(Modifier::BOLD),
            tab_active: Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            tab_inactive: Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Distinct color per overlaid series (endpoint lines in shared charts).
    pub fn series_color(&self, index: usize) -> Style {
        if self.mode == ColorMode::Mono {
            return Style::default();
        }
        const CYCLE: [Color; 6] = [
            Color::LightGreen,
            Color::LightMagenta,
            Color::Yellow,
            Color::LightCyan,
            Color::LightRed,
            Color::LightBlue,
        ];
        Style::default().fg(CYCLE[index % CYCLE.len()])
    }

    /// Style for a utilization fraction against warn/crit thresholds.
    pub fn by_level(&self, frac: f64, warn_at: f64, crit_at: f64) -> Style {
        if frac >= crit_at {
            self.crit
        } else if frac >= warn_at {
            self.warn
        } else {
            self.value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_env_wins() {
        let t = Theme::detect(false, |k| match k {
            "NO_COLOR" => Some("1".into()),
            "COLORTERM" => Some("truecolor".into()),
            _ => None,
        });
        assert_eq!(t.mode, ColorMode::Mono);
        assert_eq!(t.value, Style::default());
    }

    #[test]
    fn empty_no_color_is_ignored_per_spec() {
        let t = Theme::detect(false, |k| match k {
            "NO_COLOR" => Some(String::new()),
            _ => None,
        });
        assert_ne!(t.mode, ColorMode::Mono);
    }

    #[test]
    fn flag_wins_over_truecolor() {
        let t = Theme::detect(true, |k| match k {
            "COLORTERM" => Some("truecolor".into()),
            _ => None,
        });
        assert_eq!(t.mode, ColorMode::Mono);
    }

    #[test]
    fn level_thresholds() {
        let t = Theme::truecolor();
        assert_eq!(t.by_level(0.2, 0.7, 0.9), t.value);
        assert_eq!(t.by_level(0.75, 0.7, 0.9), t.warn);
        assert_eq!(t.by_level(0.95, 0.7, 0.9), t.crit);
    }
}
