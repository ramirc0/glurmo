//! Themes: the colors and fonts the TUI and the web view draw with.

use std::fmt::Write;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A color written as `#rrggbb`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Color(String);

impl TryFrom<String> for Color {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        let hex = text.strip_prefix('#').unwrap_or_default();
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{text:?} is not a color like #1f1d2e");
        }
        Ok(Self(text))
    }
}

impl Color {
    /// The red, green and blue channels.
    pub fn rgb(&self) -> (u8, u8, u8) {
        let channel = |at: usize| u8::from_str_radix(&self.0[at..at + 2], 16).unwrap_or_default();
        (channel(1), channel(3), channel(5))
    }
}

impl From<Color> for String {
    fn from(color: Color) -> Self {
        color.0
    }
}

/// A theme.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// The name shown in pickers.
    pub name: String,
    /// Backgrounds and text.
    pub ui: Ui,
    /// Cores that no job holds.
    pub state: States,
    /// The palette users take their colors from, by color slot.
    pub users: Vec<Color>,
    /// Font families.
    pub fonts: Fonts,
}

/// Backgrounds and text, from darkest to lightest in a dark theme.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    /// The page.
    pub base: Color,
    /// Panels.
    pub surface: Color,
    /// Raised elements.
    pub overlay: Color,
    /// Faint text.
    pub muted: Color,
    /// Secondary text.
    pub subtle: Color,
    /// Text.
    pub text: Color,
}

/// Colors for cores that no job holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct States {
    /// Free to use.
    pub free: Color,
    /// On a node that is down.
    pub down: Color,
    /// On a node that is draining or drained.
    pub drain: Color,
}

/// Font families.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fonts {
    /// For prose and labels.
    pub sans: String,
    /// For numbers and tables.
    pub mono: String,
}

impl Theme {
    /// The theme as CSS variables on `:root`: `--base` through `--text`, `--free`, `--down`, `--drain`,
    /// `--user-0` onward, `--sans` and `--mono`.
    pub fn css(&self) -> String {
        let ui = &self.ui;
        let state = &self.state;
        let mut colors = vec![
            ("base", &ui.base),
            ("surface", &ui.surface),
            ("overlay", &ui.overlay),
            ("muted", &ui.muted),
            ("subtle", &ui.subtle),
            ("text", &ui.text),
            ("free", &state.free),
            ("down", &state.down),
            ("drain", &state.drain),
        ];
        let users: Vec<String> = (0..self.users.len()).map(|i| format!("user-{i}")).collect();
        colors.extend(users.iter().map(String::as_str).zip(&self.users));
        let mut css = String::from(":root {\n");
        for (name, color) in colors {
            let _ = writeln!(css, "  --{name}: {};", color.0);
        }
        let _ = writeln!(
            css,
            "  --sans: \"{}\", system-ui, sans-serif;",
            font(&self.fonts.sans)
        );
        let _ = writeln!(
            css,
            "  --mono: \"{}\", ui-monospace, monospace;",
            font(&self.fonts.mono)
        );
        css.push_str("}\n");
        css
    }
}

/// A font family name with the characters that could end a CSS string removed.
fn font(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\n' | '<'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::from_ron;

    #[test]
    fn colors_must_be_six_hex_digits() {
        assert!(Color::try_from("#1f1d2e".to_owned()).is_ok());
        for bad in ["1f1d2e", "#1f1d2", "#1f1d2g", "red"] {
            assert!(Color::try_from(bad.to_owned()).is_err(), "{bad}");
        }
    }

    #[test]
    fn colors_split_into_channels() {
        let color = Color::try_from("#1f1D2e".to_owned()).unwrap();
        assert_eq!(color.rgb(), (0x1f, 0x1d, 0x2e));
    }

    #[test]
    fn css_names_every_color_and_font() {
        let theme: Theme = from_ron(include_str!("../themes/rose-pine.ron")).unwrap();
        let css = theme.css();
        assert!(css.starts_with(":root {\n  --base: #191724;\n"));
        assert!(css.contains("  --drain: #f6c177;\n"));
        assert!(css.contains("  --user-5: #eb6f92;\n"));
        assert!(css.contains("  --mono: \"CommitMono Nerd Font\", ui-monospace, monospace;\n"));
    }

    #[test]
    fn font_names_cannot_break_out_of_their_string() {
        assert_eq!(font("Evil\"; } body { x: \"y"), "Evil; } body { x: y");
    }
}
