//! The editor: a sidebar that builds layouts and themes on the live page, saves them in this browser,
//! and exports them as RON for the server's config directory.

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;

use crate::builder::LayoutPanel;
use crate::feed;
use crate::shelf::Shelf;
use crate::state::State;
use crate::theme_editor::ThemePanel;
use crate::tree::Path;

/// Which half of the editor shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// The panel builder.
    Layout,
    /// The theme editor.
    Theme,
}

/// What the editor tracks beside the drafts, which live in [`State`].
#[derive(Clone, Copy)]
pub struct Editor {
    /// The pane the sidebar edits.
    pub picked: RwSignal<Path>,
    /// The pane being dragged onto another to swap with it.
    pub carried: RwSignal<Option<Path>>,
    /// The file name the layout saves and exports as.
    pub layout_file: RwSignal<String>,
    /// The file name the theme saves and exports as.
    pub theme_file: RwSignal<String>,
    /// The half that shows.
    pub tab: RwSignal<Tab>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            picked: RwSignal::new(Path::root()),
            carried: RwSignal::new(None),
            layout_file: RwSignal::new(String::new()),
            theme_file: RwSignal::new(String::new()),
            tab: RwSignal::new(Tab::Layout),
        }
    }
}

impl Editor {
    /// Opens the editor on the layout and theme the page shows.
    pub fn open(self, state: State) {
        self.layout_file.set(state.layout_name.get_untracked());
        self.theme_file.set(state.theme_name.get_untracked());
        self.picked.set(Path::root());
        state.draft_theme.set(state.theme.get_untracked());
        state.draft_layout.set(state.layout.get_untracked());
    }

    /// Closes the editor. Drafts not saved are dropped.
    pub fn close(state: State) {
        state.draft_layout.set(None);
        state.draft_theme.set(None);
    }
}

/// Why `name` cannot be a file name, if it cannot. Names stay to letters, digits, `-` and `_`, so
/// they need no quoting in an address or a shell.
pub fn file_name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Give it a file name.".to_owned());
    }
    let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    (!name.chars().all(plain))
        .then(|| format!("{name:?} has characters other than letters, digits, - and _."))
}

/// A file name made from a display name, such as `rose-pine-dawn` from `Rosé Pine Dawn`.
pub fn file_name_for(name: &str) -> String {
    let mut file = String::new();
    for c in name.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_alphanumeric() {
            file.push(c);
        } else if !file.is_empty() && !file.ends_with('-') {
            file.push('-');
        }
    }
    file.trim_end_matches('-').to_owned()
}

/// The sidebar: the panel builder or the theme editor, each with its save and export block.
#[component]
pub fn Sidebar() -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let tab = move |tab: Tab, name: &'static str| {
        view! {
            <button class:on=move || editor.tab.get() == tab on:click=move |_| editor.tab.set(tab)>
                {name}
            </button>
        }
    };
    view! {
        <aside class="editor">
            <nav>
                {tab(Tab::Layout, "Layout")}
                {tab(Tab::Theme, "Theme")}
                <button class="close" title="Esc" on:click=move |_| Editor::close(state)>
                    "Close"
                </button>
            </nav>
            {move || match editor.tab.get() {
                Tab::Layout => {
                    view! {
                        <LayoutPanel/>
                        <Keep
                            shelf=Shelf::Layouts
                            file=editor.layout_file
                            draft=state.draft_layout
                            library=state.layouts
                            picked=state.layout_name
                        />
                    }
                        .into_any()
                }
                Tab::Theme => {
                    view! {
                        <ThemePanel/>
                        <Keep
                            shelf=Shelf::Themes
                            file=editor.theme_file
                            draft=state.draft_theme
                            library=state.themes
                            picked=state.theme_name
                        />
                    }
                        .into_any()
                }
            }}
        </aside>
    }
}

/// Saves `draft` in this browser as `file`, forgets a saved copy, or exports it as RON.
#[component]
fn Keep<T>(
    shelf: Shelf,
    file: RwSignal<String>,
    draft: RwSignal<Option<T>>,
    library: RwSignal<BTreeMap<String, T>>,
    picked: RwSignal<String>,
) -> impl IntoView
where
    T: Serialize + Clone + Send + Sync + 'static,
{
    let state = expect_context::<State>();
    let note = RwSignal::new(String::new());
    let ron = RwSignal::new(None::<String>);
    let changes = RwSignal::new(0_u32);
    let held = move || {
        changes.track();
        file.with(|name| shelf.holds(name))
    };
    let save = move |_| {
        let name = file.get_untracked();
        if let Some(problem) = file_name_problem(&name) {
            note.set(problem);
            return;
        }
        let Some(value) = draft.get_untracked() else {
            return;
        };
        match shelf.keep(&name, &value) {
            Ok(()) => {
                library.update(|library| {
                    library.insert(name.clone(), value);
                });
                picked.set(name.clone());
                changes.update(|count| *count += 1);
                note.set(format!("Saved as {name} in this browser."));
            }
            Err(error) => note.set(error),
        }
    };
    let forget = move |_| {
        let name = file.get_untracked();
        match shelf.forget(&name) {
            Ok(()) => {
                changes.update(|count| *count += 1);
                feed::load_library(state);
                note.set(format!("{name} is no longer saved in this browser."));
            }
            Err(error) => note.set(error),
        }
    };
    let export = move |_| {
        let Some(value) = draft.get_untracked() else {
            return;
        };
        spawn_local(async move {
            match feed::ron(shelf.kind(), &value).await {
                Ok(text) => {
                    ron.try_set(Some(text));
                }
                Err(error) => {
                    note.try_set(error);
                }
            }
        });
    };
    let exported = move || {
        ron.get().map(|text| {
            let href = format!(
                "data:text/plain;charset=utf-8,{}",
                js_sys::encode_uri_component(&text)
            );
            let name = format!("{}.ron", file.get());
            let place = format!("{}s/{name}", shelf.kind());
            let download = name.clone();
            view! {
                <textarea class="ron" readonly rows="12">
                    {text}
                </textarea>
                <p class="note">
                    <a href=href download=download>
                        "Download " {name}
                    </a>
                    " and save it as "
                    <code>{place}</code>
                    " in rtviz's config directory on the server. Every page picks it up within seconds."
                </p>
            }
        })
    };
    view! {
        <section class="keep">
            <label>
                "file name"
                <input
                    type="text"
                    spellcheck="false"
                    prop:value=move || file.get()
                    on:input=move |event| file.set(event_target_value(&event))
                />
            </label>
            <div class="buttons">
                <button on:click=save>"Save in this browser"</button>
                {move || held().then(|| view! { <button on:click=forget>"Forget saved copy"</button> })}
                <button on:click=export>"Export RON"</button>
            </div>
            <p class="note">{move || note.get()}</p>
            {exported}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_stay_plain() {
        assert_eq!(file_name_problem("tv-lab_2"), None);
        assert!(file_name_problem("").is_some());
        assert!(file_name_problem("../tv").is_some());
        assert!(file_name_problem("my tv").is_some());
    }

    #[test]
    fn display_names_become_file_names() {
        assert_eq!(file_name_for("Rosé Pine Dawn"), "ros-pine-dawn");
        assert_eq!(file_name_for("  Gruvbox (dark)  "), "gruvbox-dark");
        assert_eq!(file_name_for("---"), "");
    }
}
