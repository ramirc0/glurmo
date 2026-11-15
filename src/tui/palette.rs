//! A theme's colors as terminal colors.

use ratatui::style::Color;

use glurmo_model::theme::{self, Theme};

/// The colors one frame draws with.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    /// The page.
    pub base: Color,
    /// Raised elements, such as key chips.
    pub overlay: Color,
    /// Faint text and borders.
    pub muted: Color,
    /// Secondary text.
    pub subtle: Color,
    /// Text.
    pub text: Color,
    /// Free cores.
    pub free: Color,
    /// Cores on nodes that are down, and errors.
    pub down: Color,
    /// Cores on nodes that drain, and warnings.
    pub drain: Color,
    users: Vec<Color>,
}

impl Palette {
    /// The terminal colors of `theme`.
    pub fn of(theme: &Theme) -> Self {
        Self {
            base: rgb(&theme.ui.base),
            overlay: rgb(&theme.ui.overlay),
            muted: rgb(&theme.ui.muted),
            subtle: rgb(&theme.ui.subtle),
            text: rgb(&theme.ui.text),
            free: rgb(&theme.state.free),
            down: rgb(&theme.state.down),
            drain: rgb(&theme.state.drain),
            users: theme.users.iter().map(rgb).collect(),
        }
    }

    /// The color for palette slot `slot`, which wraps around the theme's user colors.
    pub fn user(&self, slot: u32) -> Color {
        let index = usize::try_from(slot).unwrap_or_default();
        self.users
            .get(index % self.users.len().max(1))
            .copied()
            .unwrap_or(self.text)
    }

    /// `color` moved toward the page color, keeping `keep` of it, from 0 to 1.
    pub fn fade(&self, color: Color, keep: f32) -> Color {
        let (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) = (color, self.base) else {
            return color;
        };
        let mix = |from: u8, to: u8| {
            let mixed = f32::from(from) * keep + f32::from(to) * (1.0 - keep);
            // A mix of two bytes stays within a byte.
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let byte = mixed.round().clamp(0.0, 255.0) as u8;
            byte
        };
        Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb))
    }
}

fn rgb(color: &theme::Color) -> Color {
    let (r, g, b) = color.rgb();
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::from_ron;

    fn rose_pine() -> Palette {
        Palette::of(&from_ron(include_str!("../../themes/rose-pine.ron")).unwrap())
    }

    #[test]
    fn user_slots_wrap_around_the_palette() {
        let palette = rose_pine();
        assert_eq!(palette.user(0), Color::Rgb(0x9c, 0xcf, 0xd8));
        assert_eq!(palette.user(6), palette.user(0));
    }

    #[test]
    fn fading_moves_toward_the_page() {
        let palette = rose_pine();
        assert_eq!(palette.fade(palette.text, 0.0), palette.base);
        assert_eq!(palette.fade(palette.text, 1.0), palette.text);
    }
}
