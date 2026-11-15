//! rtviz's web view: the cluster live from `rtviz serve`, laid out by the server's layouts and drawn
//! with its themes.

#![cfg_attr(
    not(target_arch = "wasm32"),
    allow(
        clippy::large_types_passed_by_value,
        reason = "State is a Copy bag of signal handles, half as large on wasm32, the only target that runs it"
    )
)]

mod die;
mod feed;
mod history;
mod paint;
mod pane;
mod panels;
mod state;

use std::collections::BTreeMap;
use std::time::Duration;

use leptos::prelude::*;
use rtviz_model::summary::holdings;

use state::{Address, State};

/// How long a wall display highlights each user.
const HIGHLIGHT_EVERY: Duration = Duration::from_secs(8);

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
    let state = State::new(Address::read());
    provide_context(state);
    feed::load_library(state);
    feed::follow(state);
    feed::watch_usage(state);
    feed::watch_fonts(state);
    paint::watch_zoom(state);
    Effect::new(move |_| {
        let css = state
            .theme
            .with(|theme| theme.as_ref().map(rtviz_model::theme::Theme::css));
        if let (Some(css), Some(style)) = (css, document().get_element_by_id("theme")) {
            style.set_text_content(Some(&css));
        }
    });
    let keys = window_event_listener(leptos::ev::keydown, move |event| {
        if event.ctrl_key() || event.meta_key() || event.alt_key() {
            return;
        }
        match event.key().as_str() {
            "t" => state
                .themes
                .with_untracked(|themes| advance(state.theme_name, themes)),
            "l" => state
                .layouts
                .with_untracked(|layouts| advance(state.layout_name, layouts)),
            _ => {}
        }
    });
    on_cleanup(move || keys.remove());
    if state.tv {
        set_interval(move || cycle_highlight(state), HIGHLIGHT_EVERY);
    }
    view! {
        <main class:tv=state.tv>
            {move || state.layout.get().map(|layout| pane::pane(layout.root))}
        </main>
    }
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
                .map(|(login, _)| login.clone())
                .collect();
            let current = state.highlight.get_untracked();
            let at = current
                .and_then(|current| holders.iter().position(|login| *login == current))
                .map_or(0, |at| at + 1);
            holders.get(at).or_else(|| holders.first()).cloned()
        })
    });
    state.highlight.set(next);
}
