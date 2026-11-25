//! The gear at the top right of the page and the menu it opens: the theme, the palette, the layout and
//! the way into the editor. The menu is a popover, so the browser closes it on a press outside it or on Esc.

use std::collections::BTreeMap;

use leptos::prelude::*;

use crate::editor::Editor;
use crate::state::{DEFAULT_LAYOUT, DEFAULT_THEME, State, pick};

/// The menu's id, which the gear names as its popover.
const MENU: &str = "settings";

/// The gear's outline in a 16 by 16 box: a ring with eight teeth around a hole.
const GEAR: &str = "M6.79 2.53L7.02 0.56A7.5 7.5 0 0 1 8.98 0.56L9.21 2.53A5.6 5.6 0 0 1 11.01 3.28\
L12.57 2.05A7.5 7.5 0 0 1 13.95 3.43L12.72 4.99A5.6 5.6 0 0 1 13.47 6.79L15.44 7.02\
A7.5 7.5 0 0 1 15.44 8.98L13.47 9.21A5.6 5.6 0 0 1 12.72 11.01L13.95 12.57\
A7.5 7.5 0 0 1 12.57 13.95L11.01 12.72A5.6 5.6 0 0 1 9.21 13.47L8.98 15.44\
A7.5 7.5 0 0 1 7.02 15.44L6.79 13.47A5.6 5.6 0 0 1 4.99 12.72L3.43 13.95\
A7.5 7.5 0 0 1 2.05 12.57L3.28 11.01A5.6 5.6 0 0 1 2.53 9.21L0.56 8.98\
A7.5 7.5 0 0 1 0.56 7.02L2.53 6.79A5.6 5.6 0 0 1 3.28 4.99L2.05 3.43\
A7.5 7.5 0 0 1 3.43 2.05L4.99 3.28A5.6 5.6 0 0 1 6.79 2.53Z\
M10.5 8A2.5 2.5 0 1 0 5.5 8A2.5 2.5 0 1 0 10.5 8Z";

/// Whether the menu is open. A browser without popovers rejects the selector, which reads as closed.
pub fn showing() -> bool {
    document()
        .query_selector(".settings:popover-open")
        .is_ok_and(|menu| menu.is_some())
}

/// The gear and its menu. The editor's sidebar takes their corner, so neither shows while it is open.
#[component]
pub fn Settings() -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    move || {
        (!state.editing.get()).then(|| {
            view! {
                <button class="gear" popovertarget=MENU title="Settings" aria-label="Settings">
                    <svg viewBox="0 0 16 16" aria-hidden="true">
                        <path fill-rule="evenodd" d=GEAR></path>
                    </svg>
                </button>
                <div id=MENU class="settings" popover="auto">
                    <Picker
                        label="theme"
                        key="t"
                        library=state.themes
                        picked=state.theme_name
                        fallback=DEFAULT_THEME
                        choose=move |name| state.theme_name.set(name)
                    />
                    <Picker
                        label="palette"
                        key="p"
                        library=state.palettes
                        picked=state.palette_name
                        fallback=""
                        unpicked="theme's own"
                        choose=move |name| state.palette_name.set(name)
                    />
                    <Picker
                        label="layout"
                        key="l"
                        library=state.layouts
                        picked=state.layout_name
                        fallback=DEFAULT_LAYOUT
                        choose=move |name| {
                            state.filled.set(None);
                            state.layout_name.set(name);
                        }
                    />
                    <div>
                        <button on:click=move |_| editor.open(state)>"Edit layout and theme"</button>
                        <kbd>"e"</kbd>
                    </div>
                </div>
            }
        })
    }
}

/// A line of the menu that picks one of `library` by name, beside the key that cycles through them. It
/// shows the entry in effect, which is `fallback` when `picked` names none. With `unpicked`, naming none
/// is a choice of its own, listed first under that label.
#[component]
fn Picker<T>(
    label: &'static str,
    key: &'static str,
    library: RwSignal<BTreeMap<String, T>>,
    picked: RwSignal<String>,
    fallback: &'static str,
    #[prop(optional)] unpicked: Option<&'static str>,
    choose: impl Fn(String) + 'static,
) -> impl IntoView
where
    T: Send + Sync + 'static,
{
    let in_effect = Memo::new(move |_| {
        library.with(|entries| match unpicked {
            Some(_) => Some(picked.get())
                .filter(|name| entries.contains_key(name))
                .or(Some(String::new())),
            None => pick(entries, &picked.get(), fallback).map(|(name, _)| name.clone()),
        })
    });
    let none = unpicked.map(|label| {
        let selected = move || in_effect.with(|name| name.as_deref() == Some(""));
        view! { <option value="" prop:selected=selected>{label}</option> }
    });
    let options = move || {
        library.with(|entries| {
            entries
                .keys()
                .map(|name| {
                    let own = name.clone();
                    // The keys change the pick too, and only the property moves a select the user
                    // has already set.
                    let selected = move || in_effect.with(|name| name.as_ref() == Some(&own));
                    view! { <option value=name.clone() prop:selected=selected>{name.clone()}</option> }
                })
                .collect_view()
        })
    };
    view! {
        <label>
            {label} <select on:change=move |event| choose(event_target_value(&event))>{none} {options}</select>
            <kbd>{key}</kbd>
        </label>
    }
}
