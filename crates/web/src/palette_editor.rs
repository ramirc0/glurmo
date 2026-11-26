//! The palette editor's half of the sidebar: what the palette colors by, the colors handed out in
//! order, names bound to a color of their own, and the color of everyone else. Every change shows on
//! the page at once.

use glurmo_model::layout::Span;
use glurmo_model::palette::{ColorBy, Deal, Palette};
use glurmo_model::theme::Color;
use glurmo_model::units::horizon;
use leptos::prelude::*;

use crate::colormap::{COLORMAPS, sampled};
use crate::editor::{Editor, file_name_for};
use crate::history::parse_span;
use crate::state::State;
use crate::theme_editor::color_input;

/// The id of the list of names a bound row suggests.
const NAMES: &str = "palette-names";

/// The color a new entry starts with.
const NEW: &str = "#9ccfd8";

/// Changes the palette being edited.
fn edit(state: State, change: impl FnOnce(&mut Palette)) {
    state.draft_palette.update(|draft| {
        if let Some(palette) = draft {
            change(palette);
        }
    });
}

/// A color to add: the last of `colors`, else [`NEW`].
fn another(colors: &[Color]) -> Color {
    colors
        .last()
        .cloned()
        .or_else(|| Color::try_from(NEW.to_owned()).ok())
        .expect("NEW is a color")
}

/// The palette editor's half of the sidebar.
#[component]
pub fn PalettePanel() -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let start_from = move |event: leptos::ev::Event| {
        let name = event_target_value(&event);
        let palette = state
            .palettes
            .with_untracked(|palettes| palettes.get(&name).cloned())
            .or_else(|| {
                state
                    .theme
                    .with_untracked(|theme| theme.as_ref().map(Palette::of_theme))
            });
        let Some(palette) = palette else {
            return;
        };
        editor.palette_file.set(if name.is_empty() {
            file_name_for(&palette.name)
        } else {
            name
        });
        state.draft_palette.set(Some(palette));
    };
    let timed = Memo::new(move |_| {
        state.draft_palette.with(|draft| {
            draft
                .as_ref()
                .is_some_and(|palette| matches!(palette.by, ColorBy::TimeLeft { .. }))
        })
    });
    view! {
        <section class="swatches">
            <label>
                "start from"
                <select on:change=start_from>
                    <option value="" selected>"…"</option>
                    {move || {
                        state
                            .palettes
                            .with(|palettes| {
                                palettes
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
                    prop:value=move || state.draft_palette.with(|draft| draft.as_ref().map(|palette| palette.name.clone()))
                    on:input=move |event| {
                        let name = event_target_value(&event);
                        edit(state, |palette| palette.name = name);
                    }
                />
            </label>
            <By/>
            <h3>{move || if timed.get() { "steps" } else { "colors" }}</h3>
            <Colors/>
            <Fill/>
            {move || (!timed.get()).then(|| view! {
                <Dealing/>
                <h3>"bound"</h3>
                <Bound/>
            })}
            <Rest/>
        </section>
    }
}

/// How the colors are handed out.
#[component]
fn Dealing() -> impl IntoView {
    let state = expect_context::<State>();
    let deal = move || {
        state
            .draft_palette
            .with(|draft| draft.as_ref().map(|palette| palette.deal))
    };
    let choose = move |event: leptos::ev::Event| {
        let deal = match event_target_value(&event).as_str() {
            "Biggest" => Deal::Biggest,
            "Spread" => Deal::Spread,
            _ => Deal::Cycle,
        };
        edit(state, |palette| palette.deal = deal);
    };
    view! {
        <label>
            "hand out"
            <select on:change=choose>
                <option value="Cycle" selected=move || deal() == Some(Deal::Cycle)>"to everyone, in turn"</option>
                <option value="Biggest" selected=move || deal() == Some(Deal::Biggest)>
                    "to the biggest holders"
                </option>
                <option value="Spread" selected=move || deal() == Some(Deal::Spread)>
                    "spread over every holder"
                </option>
            </select>
        </label>
    }
}

/// What the palette colors by, and the span when that is time left.
#[component]
fn By() -> impl IntoView {
    let state = expect_context::<State>();
    let by = move || {
        state
            .draft_palette
            .with(|draft| draft.as_ref().map(|palette| palette.by))
    };
    let timed = Memo::new(move |_| matches!(by(), Some(ColorBy::TimeLeft { .. })));
    let choose_by = move |event: leptos::ev::Event| {
        let by = match event_target_value(&event).as_str() {
            "Group" => ColorBy::Group,
            "TimeLeft" => ColorBy::TimeLeft {
                within: Span::Hours(24),
            },
            _ => ColorBy::User,
        };
        edit(state, |palette| palette.by = by);
    };
    let within = move || match by() {
        Some(ColorBy::TimeLeft { within }) => Some(horizon(within.seconds())),
        _ => None,
    };
    let set_within = move |event: leptos::ev::Event| {
        let Some(seconds) = parse_span(&event_target_value(&event)) else {
            return;
        };
        let within = match u32::try_from(seconds / 3_600) {
            Ok(hours) if seconds % 86_400 == 0 => Span::Days(hours / 24),
            Ok(hours) => Span::Hours(hours),
            Err(_) => return,
        };
        edit(state, |palette| palette.by = ColorBy::TimeLeft { within });
    };
    view! {
            <label>
                "color by"
                <select on:change=choose_by>
                    <option value="User" selected=move || by() == Some(ColorBy::User)>"users"</option>
                    <option value="Group" selected=move || by() == Some(ColorBy::Group)>"groups"</option>
                    <option value="TimeLeft" selected=move || timed.get()>"time left"</option>
                </select>
            </label>
            {move || {
                timed
                    .get()
                    .then(|| {
                        view! {
                            <label>
                                "within"
                                <input type="text" spellcheck="false" prop:value=within on:change=set_within/>
                            </label>
                            <p class="note">
                                "The colors split this span into equal steps, soonest first. A job with more left takes the last."
                            </p>
                        }
                    })
            }}
    }
}

/// The color of everyone else: a quiet one from the theme, or one picked here.
#[component]
fn Rest() -> impl IntoView {
    let state = expect_context::<State>();
    let quiet = Memo::new(move |_| {
        state
            .draft_palette
            .with(|draft| draft.as_ref().is_some_and(|palette| palette.rest.is_none()))
    });
    let rest = move || {
        (!quiet.get()).then(|| {
            color_input(
                move || {
                    state
                        .draft_palette
                        .with(|draft| draft.as_ref()?.rest.clone().map(String::from))
                },
                move |color| edit(state, |palette| palette.rest = Some(color)),
            )
        })
    };
    view! {
            <h3>"everyone else"</h3>
            <label class="check">
                <input
                    type="checkbox"
                    prop:checked=move || quiet.get()
                    on:change=move |event| {
                        let quiet = event_target_checked(&event);
                        edit(state, |palette| {
                            palette.rest = if quiet { None } else { Some(another(&[])) };
                        });
                    }
                />
                "a quiet color from the theme"
            </label>
            {rest}
    }
}

/// The colors handed out in order, each with a picker and a button to drop it.
#[component]
fn Colors() -> impl IntoView {
    let state = expect_context::<State>();
    let count = Memo::new(move |_| {
        state
            .draft_palette
            .with(|draft| draft.as_ref().map_or(0, |palette| palette.colors.len()))
    });
    let colors = move || {
        (0..count.get())
            .map(|slot| {
                let read = move || state.draft_palette.with(|draft| draft.as_ref()?.colors.get(slot).cloned().map(String::from));
                let write = move |color| {
                    edit(state, |palette| {
                        if let Some(old) = palette.colors.get_mut(slot) {
                            *old = color;
                        }
                    });
                };
                let drop = move |_| edit(state, |palette| drop(palette.colors.remove(slot)));
                view! {
                    <span class="user">
                        {color_input(read, write)} <button title="Remove this color" on:click=drop>"×"</button>
                    </span>
                }
            })
            .collect_view()
    };
    view! {
        <div class="palette">
            {colors}
            <button
                title="Add a color"
                on:click=move |_| edit(state, |palette| palette.colors.push(another(&palette.colors)))
            >
                "+"
            </button>
        </div>
        <label class="check">
            <input
                type="checkbox"
                prop:checked=move || {
                    state.draft_palette.with(|draft| draft.as_ref().is_some_and(|palette| palette.reversed))
                }
                on:change=move |event| {
                    let reversed = event_target_checked(&event);
                    edit(state, |palette| palette.reversed = reversed);
                }
            />
            "reverse the colors"
        </label>
    }
}

/// Fills the colors with samples of a colormap, brightest first.
#[component]
fn Fill() -> impl IntoView {
    let state = expect_context::<State>();
    let map = RwSignal::new(COLORMAPS[0].0.to_owned());
    let steps = RwSignal::new(6_u16);
    let fill = move |_| {
        let colors = sampled(&map.get_untracked(), steps.get_untracked());
        edit(state, |palette| palette.colors = colors);
    };
    view! {
        <label>
            "fill from"
            <span class="pair">
                <select on:change=move |event| map.set(event_target_value(&event))>
                    {COLORMAPS.iter().map(|(name, _)| view! { <option value=*name>{*name}</option> }).collect_view()}
                </select>
                <input
                    type="number"
                    min="1"
                    max="64"
                    prop:value=move || steps.get().to_string()
                    on:input=move |event| {
                        if let Ok(count) = event_target_value(&event).parse::<u16>() {
                            steps.set(count.clamp(1, 64));
                        }
                    }
                />
                <button on:click=fill>"Fill"</button>
            </span>
        </label>
    }
}

/// Names with a color of their own: a login, or a group under a palette by group.
#[component]
fn Bound() -> impl IntoView {
    let state = expect_context::<State>();
    let count = Memo::new(move |_| {
        state
            .draft_palette
            .with(|draft| draft.as_ref().map_or(0, |palette| palette.bound.len()))
    });
    let grouped = Memo::new(move |_| {
        state.draft_palette.with(|draft| {
            draft
                .as_ref()
                .is_some_and(|palette| palette.by == ColorBy::Group)
        })
    });
    let known = move || {
        let grouped = grouped.get();
        state.cluster.with(|cluster| {
            let mut names: Vec<String> = if grouped {
                cluster
                    .users
                    .values()
                    .flat_map(|user| &user.groups)
                    .map(|group| group.0.clone())
                    .collect()
            } else {
                cluster.users.keys().map(|login| login.0.clone()).collect()
            };
            names.sort();
            names.dedup();
            names
                .into_iter()
                .map(|name| view! { <option value=name></option> })
                .collect_view()
        })
    };
    let rows = move || {
        (0..count.get())
            .map(|row| {
                let read = move || state.draft_palette.with(|draft| Some(String::from(draft.as_ref()?.bound.get(row)?.1.clone())));
                let write = move |color| {
                    edit(state, |palette| {
                        if let Some(bound) = palette.bound.get_mut(row) {
                            bound.1 = color;
                        }
                    });
                };
                let drop = move |_| edit(state, |palette| drop(palette.bound.remove(row)));
                view! {
                    <span class="bound">
                        <input
                            type="text"
                            spellcheck="false"
                            list=NAMES
                            placeholder=move || if grouped.get() { "group" } else { "login" }
                            prop:value=move || state.draft_palette.with(|draft| Some(draft.as_ref()?.bound.get(row)?.0.clone()))
                            on:input=move |event| {
                                let name = event_target_value(&event);
                                edit(state, |palette| {
                                    if let Some(bound) = palette.bound.get_mut(row) {
                                        bound.0 = name;
                                    }
                                });
                            }
                        />
                        {color_input(read, write)}
                        <button title="Remove this name" on:click=drop>"×"</button>
                    </span>
                }
            })
            .collect_view()
    };
    view! {
        <datalist id=NAMES>{known}</datalist>
        {rows}
        <div class="buttons">
            <button on:click=move |_| edit(state, |palette| {
                let color = another(&palette.colors);
                palette.bound.push((String::new(), color));
            })>"Bind a name"</button>
        </div>
    }
}
