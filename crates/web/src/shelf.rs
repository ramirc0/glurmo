//! Layouts, themes and palettes saved in this browser, which add to the server's and replace them by file name.

use std::collections::BTreeMap;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Where this browser keeps one kind of saved file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shelf {
    /// Layouts.
    Layouts,
    /// Themes.
    Themes,
    /// Palettes.
    Palettes,
}

impl Shelf {
    /// The kind in the singular, as `/api/ron` and the config directory name it.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Layouts => "layout",
            Self::Themes => "theme",
            Self::Palettes => "palette",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Layouts => "glurmo.layouts",
            Self::Themes => "glurmo.themes",
            Self::Palettes => "glurmo.palettes",
        }
    }

    /// Every saved entry that still reads as a `T`, by file name. One that does not is logged.
    pub fn all<T: DeserializeOwned>(self) -> BTreeMap<String, T> {
        self.read()
            .into_iter()
            .filter_map(|(name, value)| match serde_json::from_value(value) {
                Ok(value) => Some((name, value)),
                Err(error) => {
                    let message =
                        format!("the saved {} {name:?} is unreadable: {error}", self.kind());
                    web_sys::console::warn_1(&message.into());
                    None
                }
            })
            .collect()
    }

    /// Whether an entry is saved as `name`.
    pub fn holds(self, name: &str) -> bool {
        self.read().contains_key(name)
    }

    /// Saves `value` as `name`, replacing what was there.
    pub fn keep<T: Serialize>(self, name: &str, value: &T) -> Result<(), String> {
        let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
        let mut entries = self.read();
        entries.insert(name.to_owned(), value);
        self.write(&entries)
    }

    /// Drops the entry saved as `name`.
    pub fn forget(self, name: &str) -> Result<(), String> {
        let mut entries = self.read();
        entries.remove(name);
        self.write(&entries)
    }

    fn read(self) -> BTreeMap<String, serde_json::Value> {
        storage()
            .and_then(|storage| storage.get_item(self.key()).ok().flatten())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write(self, entries: &BTreeMap<String, serde_json::Value>) -> Result<(), String> {
        let text = serde_json::to_string(entries).map_err(|error| error.to_string())?;
        storage()
            .ok_or_else(|| "this browser has no local storage".to_owned())?
            .set_item(self.key(), &text)
            .map_err(|_| "this browser refused to save it".to_owned())
    }
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}
