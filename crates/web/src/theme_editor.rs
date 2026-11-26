//! The theme editor's half of the sidebar: every color with a picker, the users' palette, the fonts,
//! and a base16 import. Every change shows on the page at once.

use glurmo_model::theme::{Color, Theme};
use leptos::prelude::*;

use crate::base16;
use crate::editor::{Editor, file_name_for};
use crate::state::State;

/// The names of the colors [`colors`] and [`colors_mut`] list, in their order.
const NAMES: [&str; 9] = [
    "base", "surface", "overlay", "muted", "subtle", "text", "free", "down", "drain",
];

/// The theme's backgrounds, text and states, in the order of [`NAMES`].
fn colors(theme: &Theme) -> [&Color; 9] {
    let (ui, state) = (&theme.ui, &theme.state);
    [
        &ui.base,
        &ui.surface,
        &ui.overlay,
        &ui.muted,
        &ui.subtle,
        &ui.text,
        &state.free,
        &state.down,
        &state.drain,
    ]
}

/// The same colors as [`colors`], to change.
fn colors_mut(theme: &mut Theme) -> [&mut Color; 9] {
    let (ui, state) = (&mut theme.ui, &mut theme.state);
    [
        &mut ui.base,
        &mut ui.surface,
        &mut ui.overlay,
        &mut ui.muted,
        &mut ui.subtle,
        &mut ui.text,
        &mut state.free,
        &mut state.down,
        &mut state.drain,
    ]
}

/// Changes the theme being edited.
fn edit(state: State, change: impl FnOnce(&mut Theme)) {
    state.draft_theme.update(|draft| {
        if let Some(theme) = draft {
            change(theme);
        }
    });
}

/// A color picker that reads with `read` and writes with `write`. It is built once and only its value
/// follows the theme, so an open picker stays open while the page repaints.
fn picker(
    state: State,
    read: impl Fn(&Theme) -> Option<String> + Send + Sync + 'static,
    write: impl Fn(&mut Theme, Color) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    color_input(
        move || {
            state
                .draft_theme
                .with(|draft| draft.as_ref().and_then(&read))
        },
        move |color| edit(state, |theme| write(theme, color)),
    )
}

/// A color picker that shows what `read` gives and hands each pick to `write`.
pub fn color_input(
    read: impl Fn() -> Option<String> + Send + Sync + 'static,
    write: impl Fn(Color) + 'static,
) -> impl IntoView {
    view! {
        <input
            type="color"
            prop:value=read
            on:input=move |event| {
                if let Ok(color) = Color::try_from(event_target_value(&event)) {
                    write(color);
                }
            }
        />
    }
}

/// The theme editor's half of the sidebar.
#[component]
pub fn ThemePanel() -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let start_from = move |event: leptos::ev::Event| {
        let name = event_target_value(&event);
        let Some(theme) = state
            .themes
            .with_untracked(|themes| themes.get(&name).cloned())
        else {
            return;
        };
        state.draft_theme.set(Some(theme));
        editor.theme_file.set(name);
    };
    let named = NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let read = move |theme: &Theme| Some(String::from(colors(theme)[index].clone()));
            let write = move |theme: &mut Theme, color: Color| *colors_mut(theme)[index] = color;
            view! { <label class="color">{picker(state, read, write)} {*name}</label> }
        })
        .collect_view();
    let font = move |mono: bool| {
        view! {
            <input
                type="text"
                spellcheck="false"
                prop:value=move || {
                    state
                        .draft_theme
                        .with(|draft| {
                            draft.as_ref().map(|theme| if mono { theme.fonts.mono.clone() } else { theme.fonts.sans.clone() })
                        })
                }
                on:input=move |event| {
                    let family = event_target_value(&event);
                    edit(state, |theme| {
                        if mono {
                            theme.fonts.mono = family;
                        } else {
                            theme.fonts.sans = family;
                        }
                    });
                }
            />
        }
    };
    view! {
        <section class="painter">
            <label>
                "start from"
                <select on:change=start_from>
                    <option value="" selected>"…"</option>
                    {move || {
                        state
                            .themes
                            .with(|themes| {
                                themes
                                    .keys()
                                    .map(|name| view! { <option value=name.clone()>{name.clone()}</option> })
                                    .collect_view()
                            })
                    }}
                </select>
            </label>
            <label>
                "name"
                <input
                    type="text"
                    prop:value=move || state.draft_theme.with(|draft| draft.as_ref().map(|theme| theme.name.clone()))
                    on:input=move |event| {
                        let name = event_target_value(&event);
                        edit(state, |theme| theme.name = name);
                    }
                />
            </label>
            <div class="colors">{named}</div>
            <h3>"users"</h3>
            <Palette/>
            <label>"sans font" {font(false)}</label>
            <label>"mono font" {font(true)}</label>
            <h3>"base16 import"</h3>
            <Import/>
        </section>
    }
}

/// The colors users take, by color slot, each with a picker and a button to drop it.
#[component]
fn Palette() -> impl IntoView {
    let state = expect_context::<State>();
    let users = Memo::new(move |_| {
        state
            .draft_theme
            .with(|draft| draft.as_ref().map_or(0, |theme| theme.users.len()))
    });
    let palette = move || {
        (0..users.get())
            .map(|slot| {
                let read = move |theme: &Theme| theme.users.get(slot).cloned().map(String::from);
                let write = move |theme: &mut Theme, color: Color| {
                    if let Some(user) = theme.users.get_mut(slot) {
                        *user = color;
                    }
                };
                let drop = move |_| {
                    edit(state, |theme| {
                        if theme.users.len() > 1 {
                            theme.users.remove(slot);
                        }
                    });
                };
                view! {
                    <span class="user">
                        {picker(state, read, write)}
                        <button title="Remove this color" on:click=drop>"×"</button>
                    </span>
                }
            })
            .collect_view()
    };
    let add = move |_| {
        edit(state, |theme| {
            let next = theme
                .users
                .last()
                .cloned()
                .unwrap_or_else(|| theme.ui.text.clone());
            theme.users.push(next);
        });
    };
    view! {
        <div class="palette">
            {palette}
            <button title="Add a color" on:click=add>"+"</button>
        </div>
    }
}

/// A box to paste a base16 scheme into, which replaces the draft's colors and keeps its fonts.
#[component]
fn Import() -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let scheme = RwSignal::new(String::new());
    let problem = RwSignal::new(String::new());
    let import = move |_| {
        let Some(fonts) = state
            .draft_theme
            .with_untracked(|draft| draft.as_ref().map(|theme| theme.fonts.clone()))
        else {
            return;
        };
        match base16::theme(&scheme.get_untracked(), fonts) {
            Ok(theme) => {
                editor.theme_file.set(file_name_for(&theme.name));
                state.draft_theme.set(Some(theme));
                problem.set(String::new());
            }
            Err(error) => problem.set(error),
        }
    };
    view! {
        <textarea
            class="scheme"
            rows="5"
            spellcheck="false"
            placeholder="Paste a base16 scheme's YAML"
            prop:value=move || scheme.get()
            on:input=move |event| scheme.set(event_target_value(&event))
        ></textarea>
        <div class="buttons">
            <button on:click=import>"Import"</button>
        </div>
        <p class="note down">{move || problem.get()}</p>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_color_lists_name_the_same_fields() {
        let scheme = (0..16)
            .map(|i| format!("base0{i:X}: \"{:06x}\"", i * 0x10_1010))
            .collect::<Vec<_>>()
            .join("\n");
        let fonts = glurmo_model::theme::Fonts {
            sans: "a".into(),
            mono: "b".into(),
        };
        let mut theme = base16::theme(&scheme, fonts).unwrap();
        let read: Vec<Color> = colors(&theme).into_iter().cloned().collect();
        let written: Vec<Color> = colors_mut(&mut theme)
            .into_iter()
            .map(|color| color.clone())
            .collect();
        assert_eq!(read, written);
        assert_eq!(read[0], theme.ui.base);
        assert_eq!(read[8], theme.state.drain);
    }
}
