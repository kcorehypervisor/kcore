//! Color palette aligned with `assets/kcore-logo.png`: black field, violet accent, silver text.

use ratatui::style::{Color, Modifier, Style};

pub fn bg() -> Color {
    Color::Rgb(0, 0, 0)
}

pub fn accent() -> Color {
    // Dominant violet from the shipped README / console logo art (sampled from PNG)
    Color::Rgb(83, 90, 244)
}

/// Highlight / metallic tone for wordmarks (logo silver).
pub fn silver() -> Color {
    Color::Rgb(210, 212, 220)
}

pub fn text() -> Color {
    Color::Rgb(228, 230, 235)
}

pub fn muted() -> Color {
    Color::Rgb(118, 120, 132)
}

/// Selected row background (tables).
pub fn selection_bg() -> Color {
    Color::Rgb(22, 22, 38)
}

pub fn good() -> Color {
    Color::Rgb(80, 200, 120)
}

pub fn warn() -> Color {
    Color::Rgb(255, 200, 100)
}

pub fn bad() -> Color {
    Color::Rgb(255, 100, 100)
}

pub fn title_style() -> Style {
    Style::default().fg(accent()).add_modifier(Modifier::BOLD)
}

pub fn health_style(s: &str) -> Style {
    let c = if s == "OK" {
        good()
    } else if s == "Degraded" {
        warn()
    } else if s == "Critical" {
        bad()
    } else {
        warn()
    };
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}
