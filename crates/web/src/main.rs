//! glurmo's web view: the cluster live from `glurmo serve`, laid out by the server's layouts and drawn
//! with its themes.

#![allow(
    clippy::large_types_passed_by_value,
    reason = "State is a Copy bag of signal handles, which Leptos passes around by value"
)]

mod base16;
mod builder;
mod colormap;
mod editor;
mod feed;
mod history;
mod jobs;
mod paint;
mod palette_editor;
mod pane;
mod panels;
mod replay;
mod settings;
mod shelf;
mod state;
mod theme_editor;
mod tiles;
mod tip;
mod tree;

use std::collections::BTreeMap;
use std::time::Duration;

use glurmo_model::summary::holdings;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use editor::{Editor, Sidebar};
use settings::Settings;
use state::{Address, Highlight, State};
use tree::Path;

/// How long a wall display highlights each user.
const HIGHLIGHT_EVERY: Duration = Duration::from_secs(8);

/// How often the header clock ticks.
const TICK: Duration = Duration::from_secs(1);

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let error = js_sys::Error::new(&format!("panic: {info}"));
        let stack = js_sys::Reflect::get(&error, &"stack".into()).unwrap_or_else(|_| error.into());
        web_sys::console::error_1(&stack);
    }));
    leptos::mount::mount_to_body(App);
}

/// The page: the picked layout, filled from the server's stream.
#[component]
fn App() -> impl IntoView {
    let address = Address::read();
    let (at, speed) = (address.at, address.speed);
    let state = State::new(address);
    let editor = Editor::default();
    provide_context(state);
    provide_context(editor);
    feed::load_library(state);
    feed::follow(state);
    feed::watch_usage(state);
    feed::watch_fonts(state);
    paint::watch_zoom(state);
    set_interval(move || state.now.set(paint::now()), TICK);
    if let Some(at) = at {
        replay::seek(state, at);
        replay::play(state, speed);
    }
    Effect::new(move |_| {
        let css = state
            .theme
            .with(|theme| theme.as_ref().map(glurmo_model::theme::Theme::css));
        if let (Some(css), Some(style)) = (css, document().get_element_by_id("theme")) {
            style.set_text_content(Some(&css));
        }
    });
    let keys = window_event_listener(leptos::ev::keydown, move |event| {
        if event.ctrl_key() || event.meta_key() || event.alt_key() || typing(&event) {
            return;
        }
        let editing = state.draft_layout.with_untracked(Option::is_some);
        let replaying = state.replay.with_untracked(Option::is_some);
        let filled = state.filled.with_untracked(Option::is_some);
        match event.key().as_str() {
            // The browser closes the menu on this press.
            "Escape" if settings::showing() => {}
            "e" | "Escape" if editing => Editor::close(state),
            "f" | "Escape" if filled => state.filled.set(None),
            "f" if !editing => state.filled.set(state.pointed.get_value()),
            "Escape" if replaying => replay::live(state),
            "0" if replaying => replay::play(state, None),
            key @ ("1" | "2" | "3" | "4") if replaying => {
                let speed = key
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| replay::SPEEDS.get(n - 1));
                replay::play(state, speed.copied());
            }
            "e" if !state.tv => editor.open(state),
            "t" if !editing => state
                .themes
                .with_untracked(|themes| advance(state.theme_name, themes)),
            "p" if !editing => state.palettes.with_untracked(|palettes| {
                let picked = state.palette_name.get_untracked();
                let next = palettes.keys().find(|name| **name > picked);
                state.palette_name.set(next.cloned().unwrap_or_default());
            }),
            "l" if !editing => {
                state.filled.set(None);
                state
                    .layouts
                    .with_untracked(|layouts| advance(state.layout_name, layouts));
            }
            _ => {}
        }
    });
    on_cleanup(move || keys.remove());
    if state.tv {
        set_interval(move || cycle_highlight(state), HIGHLIGHT_EVERY);
    }
    // The stage rebuilds when the panes change, not when the draft's name does.
    let root = Memo::new(move |_| {
        state
            .layout
            .with(|layout| layout.as_ref().map(|layout| layout.root.clone()))
    });
    view! {
        <main class:tv=state.tv class:editing=move || state.editing.get()>
            <div class="stage">
                {move || {
                    let editing = state.editing.get();
                    let root = root.get()?;
                    let filled = state
                        .filled
                        .get()
                        .and_then(|path| Some((tree::at(&root, &path)?.clone(), path)));
                    Some(match filled {
                        Some((pane, path)) => pane::pane(pane, path, false),
                        None => pane::pane(root, Path::root(), editing),
                    })
                }}
            </div>
            {move || state.editing.get().then(|| view! { <Sidebar/> })}
            {(!state.tv).then(|| view! { <Settings/> })}
        </main>
    }
}

/// Whether a key went to a form field, where it is typing rather than a command.
fn typing(event: &web_sys::KeyboardEvent) -> bool {
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .is_some_and(|element| {
            matches!(element.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")
        })
}

/// Sets `picked` to the name after it in `among`, in name order, wrapping around.
fn advance<T>(picked: RwSignal<String>, among: &BTreeMap<String, T>) {
    let current = picked.get_untracked();
    let next = among
        .keys()
        .find(|name| **name > current)
        .or_else(|| among.keys().next());
    if let Some(next) = next {
        picked.set(next.clone());
    }
}

/// Moves the highlight to the next user who holds cores, by most cores first, or clears it when no one
/// holds any.
fn cycle_highlight(state: State) {
    let next = state.cluster.with_untracked(|cluster| {
        state.occupancy.with_untracked(|occupancy| {
            let holders: Vec<_> = holdings(cluster, occupancy)
                .users
                .into_iter()
                .filter(|(_, held)| held.cores + held.gpus > 0)
                .map(|(login, _)| Highlight::User(login.clone()))
                .collect();
            let current = state.highlight.get_untracked();
            let at = current
                .and_then(|current| holders.iter().position(|held| *held == current))
                .map_or(0, |at| at + 1);
            holders.get(at).or_else(|| holders.first()).cloned()
        })
    });
    state.highlight.set(next);
}
