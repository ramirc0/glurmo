//! Themes from base16 color schemes, in the classic flat form or tinted-theming's `palette:` form.

use std::collections::BTreeMap;

use rtviz_model::theme::{Color, Fonts, States, Theme, Ui};

/// The theme a base16 scheme describes, drawn in `fonts`.
///
/// `base00` to `base05` run from the background to the text, as rtviz's UI colors do. Free cores take
/// the selection background `base02`, down nodes the red `base08` and draining ones the yellow `base0A`.
/// Users take the remaining accents, so none of them looks like a node in trouble.
pub fn theme(scheme: &str, fonts: Fonts) -> Result<Theme, String> {
    let mut name = None;
    let mut colors = BTreeMap::new();
    for line in scheme.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = unquote(value);
        if key == "scheme" || key == "name" {
            name = Some(value.to_owned());
        } else if key.starts_with("base") {
            let hex = value.trim_start_matches('#').to_ascii_lowercase();
            let color =
                Color::try_from(format!("#{hex}")).map_err(|error| format!("{key}: {error}"))?;
            colors.insert(key, color);
        }
    }
    let color = |key: &str| {
        colors
            .get(key)
            .cloned()
            .ok_or_else(|| format!("the scheme has no {key}"))
    };
    Ok(Theme {
        name: name.unwrap_or_else(|| "base16".to_owned()),
        ui: Ui {
            base: color("base00")?,
            surface: color("base01")?,
            overlay: color("base02")?,
            muted: color("base03")?,
            subtle: color("base04")?,
            text: color("base05")?,
        },
        state: States {
            free: color("base02")?,
            down: color("base08")?,
            drain: color("base0a")?,
        },
        users: ["base0c", "base0e", "base0d", "base0b", "base09", "base0f"]
            .iter()
            .map(|key| color(key))
            .collect::<Result<_, _>>()?,
        fonts,
    })
}

/// A YAML scalar without its quotes or a trailing comment.
fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            return rest.split(quote).next().unwrap_or_default();
        }
    }
    value.split([' ', '#']).next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts {
            sans: "Hanken Grotesk".into(),
            mono: "CommitMono Nerd Font".into(),
        }
    }

    const CLASSIC: &str = r#"
scheme: "Rosé Pine"
author: "Emilia Dunfelt <edun@dunfelt.se>"
base00: "191724"
base01: "1f1d2e"
base02: "26233a"
base03: "6e6a86" # comments
base04: "908caa"
base05: "e0def4"
base06: "e0def4"
base07: "524f67"
base08: "eb6f92"
base09: "f6c177"
base0A: "ebbcba"
base0B: "31748f"
base0C: "9ccfd8"
base0D: "c4a7e7"
base0E: "f6c177"
base0F: "524f67"
"#;

    #[test]
    fn a_classic_scheme_maps_onto_a_theme() {
        let theme = theme(CLASSIC, fonts()).unwrap();
        assert_eq!(theme.name, "Rosé Pine");
        assert_eq!(theme.ui.base.rgb(), (0x19, 0x17, 0x24));
        assert_eq!(theme.ui.muted.rgb(), (0x6e, 0x6a, 0x86));
        assert_eq!(theme.state.free, theme.ui.overlay);
        assert_eq!(theme.state.down.rgb(), (0xeb, 0x6f, 0x92));
        assert_eq!(theme.state.drain.rgb(), (0xeb, 0xbc, 0xba));
        assert_eq!(theme.users.len(), 6);
        assert_eq!(theme.users[0].rgb(), (0x9c, 0xcf, 0xd8));
        assert_eq!(theme.fonts, fonts());
    }

    #[test]
    fn a_tinted_theming_scheme_with_hashes_and_a_palette_maps_too() {
        let palette = CLASSIC
            .lines()
            .filter(|line| line.starts_with("base"))
            .map(|line| format!("  {}", line.replace(": \"", ": \"#")))
            .collect::<Vec<_>>()
            .join("\n");
        let tinted = format!(
            "system: \"base16\"\nname: \"Rosé Pine\"\nvariant: \"dark\"\npalette:\n{palette}"
        );
        let theme = theme(&tinted, fonts()).unwrap();
        assert_eq!(theme.name, "Rosé Pine");
        assert_eq!(theme.ui.text.rgb(), (0xe0, 0xde, 0xf4));
    }

    #[test]
    fn a_missing_or_bad_color_is_named() {
        let short = CLASSIC.replace("base0C: \"9ccfd8\"\n", "");
        assert_eq!(
            theme(&short, fonts()).unwrap_err(),
            "the scheme has no base0c"
        );
        let bad = CLASSIC.replace("191724", "1917");
        assert!(theme(&bad, fonts()).unwrap_err().starts_with("base00: "));
    }

    #[test]
    fn quotes_and_comments_come_off() {
        assert_eq!(unquote(" \"#191724\" # dark"), "#191724");
        assert_eq!(unquote(" '191724'"), "191724");
        assert_eq!(unquote(" 191724 # dark"), "191724");
    }
}
