//! Themes, palettes and layouts: the built-in ones, plus `*.ron` files under the config directory that add to
//! them or replace them by file name.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::from_ron;
use glurmo_model::layout::Layout;
use glurmo_model::palette::Palette;
use glurmo_model::theme::Theme;

const THEMES: [(&str, &str); 23] = [
    ("rose-pine", include_str!("../themes/rose-pine.ron")),
    (
        "rose-pine-moon",
        include_str!("../themes/rose-pine-moon.ron"),
    ),
    (
        "rose-pine-dawn",
        include_str!("../themes/rose-pine-dawn.ron"),
    ),
    ("ayu-dark", include_str!("../themes/ayu-dark.ron")),
    ("ayu-light", include_str!("../themes/ayu-light.ron")),
    (
        "catppuccin-frappe",
        include_str!("../themes/catppuccin-frappe.ron"),
    ),
    (
        "catppuccin-latte",
        include_str!("../themes/catppuccin-latte.ron"),
    ),
    (
        "catppuccin-macchiato",
        include_str!("../themes/catppuccin-macchiato.ron"),
    ),
    (
        "catppuccin-mocha",
        include_str!("../themes/catppuccin-mocha.ron"),
    ),
    ("github", include_str!("../themes/github.ron")),
    ("github-dark", include_str!("../themes/github-dark.ron")),
    (
        "github-dark-colorblind",
        include_str!("../themes/github-dark-colorblind.ron"),
    ),
    (
        "github-light-colorblind",
        include_str!("../themes/github-light-colorblind.ron"),
    ),
    ("gruvbox-dark", include_str!("../themes/gruvbox-dark.ron")),
    ("gruvbox-light", include_str!("../themes/gruvbox-light.ron")),
    ("hardhacker", include_str!("../themes/hardhacker.ron")),
    ("miami", include_str!("../themes/miami.ron")),
    ("onedark", include_str!("../themes/onedark.ron")),
    ("onedark-dark", include_str!("../themes/onedark-dark.ron")),
    ("outrun-dark", include_str!("../themes/outrun-dark.ron")),
    (
        "tokyo-night-dark",
        include_str!("../themes/tokyo-night-dark.ron"),
    ),
    (
        "tokyo-night-light",
        include_str!("../themes/tokyo-night-light.ron"),
    ),
    ("x", include_str!("../themes/x.ron")),
];

const LAYOUTS: [(&str, &str); 4] = [
    ("tui", include_str!("../layouts/tui.ron")),
    ("web", include_str!("../layouts/web.ron")),
    ("tv", include_str!("../layouts/tv.ron")),
    ("jobs", include_str!("../layouts/jobs.ron")),
];

const PALETTES: [(&str, &str); 5] = [
    ("mono", include_str!("../palettes/mono.ron")),
    ("okabe-ito", include_str!("../palettes/okabe-ito.ron")),
    ("groups", include_str!("../palettes/groups.ron")),
    ("viridis", include_str!("../palettes/viridis.ron")),
    ("time-left", include_str!("../palettes/time-left.ron")),
];

/// Every theme, palette and layout, by file name without `.ron`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Library {
    /// Themes.
    pub themes: BTreeMap<String, Theme>,
    /// Palettes.
    pub palettes: BTreeMap<String, Palette>,
    /// Layouts.
    pub layouts: BTreeMap<String, Layout>,
}

/// When each file in a library was last changed, to notice edits by polling.
/// Polling also sees edits made from other hosts on a network filesystem, which inotify misses.
pub type Stamp = Vec<(PathBuf, Option<SystemTime>, u64)>;

impl Library {
    /// The built-ins, with `dir/themes/*.ron`, `dir/palettes/*.ron` and `dir/layouts/*.ron` on top. A file that does not parse
    /// is reported on stderr and skipped.
    pub fn load(dir: &Path) -> Self {
        Self {
            themes: shelve(&THEMES, &dir.join("themes")),
            palettes: shelve(&PALETTES, &dir.join("palettes")),
            layouts: shelve(&LAYOUTS, &dir.join("layouts")),
        }
    }

    /// The library files under `dir` and when each last changed. A different stamp means a file changed.
    pub fn stamp(dir: &Path) -> Stamp {
        let mut stamp: Stamp = ["themes", "palettes", "layouts"]
            .iter()
            .flat_map(|kind| ron_files(&dir.join(kind)))
            .map(|path| {
                let meta = fs::metadata(&path).ok();
                let modified = meta.as_ref().and_then(|meta| meta.modified().ok());
                let len = meta.map_or(0, |meta| meta.len());
                (path, modified, len)
            })
            .collect();
        stamp.sort();
        stamp
    }
}

/// Checks the files under `dir` every 2 s, and loads the library into `library` again when one changed.
pub async fn keep_current(dir: &Path, library: &tokio::sync::watch::Sender<Library>) {
    let mut stamp = Library::stamp(dir);
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        ticks.tick().await;
        let now = Library::stamp(dir);
        if now != stamp {
            stamp = now;
            library.send_replace(Library::load(dir));
        }
    }
}

/// Parses the built-ins, then every `*.ron` file in `dir`, keyed by name.
fn shelve<T: DeserializeOwned>(builtins: &[(&str, &str)], dir: &Path) -> BTreeMap<String, T> {
    let builtins = builtins
        .iter()
        .map(|&(name, text)| (name.to_owned(), Ok(text.to_owned()), name.to_owned()));
    let files = ron_files(dir).into_iter().map(|path| {
        let name = path
            .file_stem()
            .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
        (name, fs::read_to_string(&path), path.display().to_string())
    });
    let mut shelf = BTreeMap::new();
    for (name, text, source) in builtins.chain(files) {
        match text
            .map_err(anyhow::Error::from)
            .and_then(|text| from_ron(&text))
        {
            Ok(value) => {
                shelf.insert(name, value);
            }
            Err(error) => tracing::warn!("{source} skipped: {error:#}"),
        }
    }
    shelf
}

/// The `*.ron` files in `dir`, or none when it does not exist.
fn ron_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "ron"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glurmo_model::layout::Pane;
    use glurmo_model::theme::DEFAULT_THEME;

    #[test]
    fn built_ins_parse() {
        let library = Library::load(Path::new("/nonexistent"));
        assert_eq!(library.themes.len(), THEMES.len());
        assert_eq!(library.palettes.len(), PALETTES.len());
        assert_eq!(library.layouts.len(), LAYOUTS.len());
        assert!(matches!(library.layouts["tui"].root, Pane::Split(..)));
        assert_eq!(library.themes[DEFAULT_THEME].name, "Miami");
    }

    #[test]
    fn files_add_to_and_replace_built_ins_and_bad_ones_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("themes")).unwrap();
        let dawn = THEMES[2].1.replace("Rosé Pine Dawn", "Mine");
        fs::write(dir.path().join("themes/rose-pine.ron"), &dawn).unwrap();
        fs::write(dir.path().join("themes/extra.ron"), &dawn).unwrap();
        fs::write(dir.path().join("themes/broken.ron"), "Theme(").unwrap();
        fs::write(dir.path().join("themes/notes.txt"), "not a theme").unwrap();
        let library = Library::load(dir.path());
        assert!(library.themes.contains_key("extra"));
        assert!(!library.themes.contains_key("broken"));
        assert_eq!(library.themes.len(), THEMES.len() + 1);
        assert_eq!(library.themes["rose-pine"].name, "Mine");
    }

    #[test]
    fn stamp_changes_when_a_file_does() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Library::stamp(dir.path()), []);
        fs::create_dir(dir.path().join("layouts")).unwrap();
        fs::write(dir.path().join("layouts/a.ron"), LAYOUTS[0].1).unwrap();
        let before = Library::stamp(dir.path());
        assert_eq!(before.len(), 1);
        fs::write(dir.path().join("layouts/a.ron"), LAYOUTS[1].1).unwrap();
        assert_ne!(Library::stamp(dir.path()), before);
    }
}
