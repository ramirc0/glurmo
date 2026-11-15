//! The panel builder: covers to pick panels by and drag them onto each other to swap them, grips to
//! resize panes by, and the sidebar half that edits the picked pane.

use leptos::prelude::*;
use rtviz_model::layout::{Direction, Grouping, Pane, QueueBy, Size, Span};
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, PointerEvent};

use crate::editor::Editor;
use crate::state::State;
use crate::tree::{self, Path, at, at_mut, dragged, size_mut};

/// Every panel, with the options a new one starts with.
const PANELS: [Pane; 11] = [
    Pane::Header,
    Pane::Die {
        group: Grouping::Hardware,
    },
    Pane::Users,
    Pane::Queue {
        by: QueueBy::Partition,
    },
    Pane::Partitions,
    Pane::Nodes,
    Pane::History {
        span: Span::Hours(24),
    },
    Pane::Mine,
    Pane::Jobs,
    Pane::Ticker,
    Pane::Keys,
];

/// The smallest a pane may be dragged to, in CSS pixels.
const MIN_PANE: f64 = 24.0;

/// Changes the layout being edited.
pub fn edit(state: State, change: impl FnOnce(&mut Pane)) {
    state.draft_layout.update(|draft| {
        if let Some(layout) = draft {
            change(&mut layout.root);
        }
    });
}

/// The kind of pane, as the layout files name it.
fn kind(pane: &Pane) -> &'static str {
    match pane {
        Pane::Split(..) => "Split",
        Pane::Header => "Header",
        Pane::Keys => "Keys",
        Pane::Die { .. } => "Die",
        Pane::Users => "Users",
        Pane::Queue { .. } => "Queue",
        Pane::Partitions => "Partitions",
        Pane::Nodes => "Nodes",
        Pane::History { .. } => "History",
        Pane::Mine => "Mine",
        Pane::Jobs => "Jobs",
        Pane::Ticker => "Ticker",
    }
}

/// The pane's kind and options in a few words.
pub fn label(pane: &Pane) -> String {
    match pane {
        Pane::Split(Direction::Horizontal, children) => format!("{} side by side", children.len()),
        Pane::Split(Direction::Vertical, children) => format!("{} stacked", children.len()),
        Pane::Die { group } => format!("Die by {}", words(*group)),
        Pane::Queue {
            by: QueueBy::Partition,
        } => "Queue by partition".to_owned(),
        Pane::Queue { by: QueueBy::Login } => "Queue by user".to_owned(),
        Pane::History {
            span: Span::Hours(hours),
        } => format!("History, {hours} h"),
        Pane::History {
            span: Span::Days(days),
        } => format!("History, {days} d"),
        other => kind(other).to_owned(),
    }
}

fn words(group: Grouping) -> &'static str {
    match group {
        Grouping::Hardware => "hardware",
        Grouping::Partition => "partition",
    }
}

/// What the sidebar needs to know to choose its controls. It changes far less often than the
/// layout, so controls are not rebuilt, and do not lose focus, on every edit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Picked {
    path: Path,
    kind: &'static str,
}

/// A panel's cover while editing, tagged `tag`. Clicking picks the panel. Dragging it onto another
/// swaps them.
#[component]
pub fn Cover(tag: String, path: Path) -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let here = StoredValue::new(path);
    view! {
        <div
            class="cover"
            class:picked=move || here.with_value(|here| editor.picked.with(|picked| picked == here))
            class:target=move || {
                editor.carried.with(|carried| carried.as_ref().is_some_and(|from| here.with_value(|here| from != here)))
            }
            draggable="true"
            on:click=move |_| editor.picked.set(here.get_value())
            on:dragstart=move |event| {
                if let Some(transfer) = event.data_transfer() {
                    let _ = transfer.set_data("text/plain", "pane");
                }
                editor.carried.set(Some(here.get_value()));
            }
            on:dragend=move |_| editor.carried.set(None)
            on:dragover=move |event| event.prevent_default()
            on:drop=move |event| {
                event.prevent_default();
                let to = here.get_value();
                if let Some(from) = editor.carried.get_untracked() {
                    edit(state, |root| {
                        tree::swap(root, &from, &to);
                    });
                    editor.picked.set(to);
                }
                editor.carried.set(None);
            }
        >
            <span class="tag">{tag}</span>
        </div>
    }
}

/// A drag in progress: where it began, the split's panes with their own styles, and their lengths.
struct Drag {
    from: f64,
    slots: Vec<HtmlElement>,
    styles: Vec<Option<String>>,
    before: Vec<f64>,
}

/// The grip in the gap after pane `gap` of the split at `split`. Dragging it moves the gap. The two
/// panes beside it follow the pointer, and the layout takes their new sizes on release.
#[component]
pub fn Grip(split: Path, gap: usize, direction: Direction) -> impl IntoView {
    let state = expect_context::<State>();
    let split = StoredValue::new(split);
    let drag = StoredValue::new_local(None::<Drag>);
    let along = move |event: &PointerEvent| {
        f64::from(match direction {
            Direction::Horizontal => event.client_x(),
            Direction::Vertical => event.client_y(),
        })
    };
    let length = move |element: &HtmlElement| {
        let rect = element.get_bounding_client_rect();
        match direction {
            Direction::Horizontal => rect.width(),
            Direction::Vertical => rect.height(),
        }
    };
    let start = move |event: PointerEvent| {
        let Some(grip) = event
            .current_target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let Some(container) = grip.parent_element().and_then(|slot| slot.parent_element()) else {
            return;
        };
        let children = container.children();
        let slots: Vec<HtmlElement> = (0..children.length())
            .filter_map(|index| children.item(index)?.dyn_into().ok())
            .collect();
        if slots.len() <= gap + 1 {
            return;
        }
        event.prevent_default();
        let _ = grip.set_pointer_capture(event.pointer_id());
        drag.set_value(Some(Drag {
            from: along(&event),
            before: slots.iter().map(length).collect(),
            styles: slots
                .iter()
                .map(|slot| slot.get_attribute("style"))
                .collect(),
            slots,
        }));
    };
    let pull = move |event: PointerEvent| {
        drag.with_value(|drag| {
            let Some(drag) = drag else {
                return;
            };
            let moved = along(&event) - drag.from;
            let lengths = [drag.before[gap] + moved, drag.before[gap + 1] - moved];
            if lengths.iter().any(|length| *length < MIN_PANE) {
                return;
            }
            for (slot, length) in drag.slots[gap..=gap + 1].iter().zip(lengths) {
                let _ = slot
                    .style()
                    .set_property("flex", &format!("0 0 {length}px"));
            }
        });
    };
    let finish = move |_: PointerEvent| {
        let mut taken = None;
        drag.update_value(|drag| taken = drag.take());
        let Some(drag) = taken else {
            return;
        };
        let after: Vec<f64> = drag.slots.iter().map(length).collect();
        for (slot, style) in drag.slots.iter().zip(&drag.styles) {
            let _ = match style {
                Some(style) => slot.set_attribute("style", style),
                None => slot.remove_attribute("style"),
            };
        }
        edit(state, |root| {
            let path = split.get_value();
            if let Some(Pane::Split(_, children)) = at_mut(root, &path) {
                let sizes: Vec<Size> = children.iter().map(|(size, _)| *size).collect();
                let resized = dragged(&sizes, gap, &drag.before, &after);
                for ((size, _), new) in children.iter_mut().zip(resized) {
                    *size = new;
                }
            }
        });
    };
    let class = match direction {
        Direction::Horizontal => "grip across",
        Direction::Vertical => "grip down",
    };
    view! {
        <div
            class=class
            on:pointerdown=start
            on:pointermove=pull
            on:pointerup=finish
            on:pointercancel=finish
        ></div>
    }
}

/// The builder's half of the sidebar: where to start, the layout's name, and the picked pane.
#[component]
pub fn LayoutPanel() -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let start_from = move |event: leptos::ev::Event| {
        let name = event_target_value(&event);
        let Some(layout) = state
            .layouts
            .with_untracked(|layouts| layouts.get(&name).cloned())
        else {
            return;
        };
        state.draft_layout.set(Some(layout));
        editor.layout_file.set(name);
        editor.picked.set(Path::root());
    };
    let picked = Memo::new(move |_| {
        let path = editor.picked.get();
        state.draft_layout.with(|draft| {
            let pane = at(&draft.as_ref()?.root, &path)?;
            Some(Picked {
                kind: kind(pane),
                path,
            })
        })
    });
    view! {
        <section class="builder">
            <label>
                "start from"
                <select on:change=start_from>
                    <option value="" selected>"…"</option>
                    {move || {
                        state
                            .layouts
                            .with(|layouts| {
                                layouts
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
                    prop:value=move || {
                        state.draft_layout.with(|draft| draft.as_ref().map(|layout| layout.name.clone()))
                    }
                    on:input=move |event| {
                        let name = event_target_value(&event);
                        state.draft_layout.update(|draft| {
                            if let Some(layout) = draft {
                                layout.name = name;
                            }
                        });
                    }
                />
            </label>
            {move || {
                picked
                    .get()
                    .map_or_else(
                        || view! { <p class="note">"Click a panel to edit it."</p> }.into_any(),
                        |picked| view! { <PaneControls picked=picked/> }.into_any(),
                    )
            }}
            <p class="note">
                "Drag a panel onto another to swap them. Drag the gap between two panes to resize them."
            </p>
        </section>
    }
}

/// Controls for the picked pane: where it sits, its size, what it shows, and splitting or removing it.
#[component]
fn PaneControls(picked: Picked) -> impl IntoView {
    let state = expect_context::<State>();
    let editor = expect_context::<Editor>();
    let path = StoredValue::new(picked.path.clone());
    let pane = move || {
        state.draft_layout.with(|draft| {
            draft
                .as_ref()
                .and_then(|layout| path.with_value(|path| at(&layout.root, path).cloned()))
        })
    };
    let lineage = picked
        .path
        .lineage()
        .into_iter()
        .map(|step| {
            let here = step == picked.path;
            let shown = StoredValue::new(step.clone());
            let name = move || {
                state.draft_layout.with(|draft| {
                    draft.as_ref().and_then(|layout| {
                        shown.with_value(|step| at(&layout.root, step).map(label))
                    })
                })
            };
            view! {
                <button class="crumb" class:on=here on:click=move |_| editor.picked.set(step.clone())>
                    {name}
                </button>
            }
        })
        .collect_view();
    let split = move |direction: Direction| {
        let mut copy = None;
        edit(state, |root| {
            copy = path.with_value(|path| tree::split(root, path, direction));
        });
        if let Some(copy) = copy {
            editor.picked.set(copy);
        }
    };
    let remove = move |_| {
        let mut parent = None;
        edit(state, |root| {
            parent = path.with_value(|path| tree::remove(root, path));
        });
        if let Some(parent) = parent {
            editor.picked.set(parent);
        }
    };
    let is_root = picked.path == Path::root();
    view! {
        <div class="crumbs">{lineage}</div>
        {(!is_root).then(|| view! { <SizeControl path=picked.path.clone()/> })}
        {if picked.kind == "Split" {
            view! { <DirectionControl path=picked.path.clone()/> }.into_any()
        } else {
            view! { <PanelControl path=picked.path.clone() pane=pane/> }.into_any()
        }}
        <div class="buttons">
            <button on:click=move |_| split(Direction::Horizontal)>"Split side by side"</button>
            <button on:click=move |_| split(Direction::Vertical)>"Split stacked"</button>
            <button disabled=is_root on:click=remove>
                "Remove"
            </button>
        </div>
    }
}

/// The picked pane's size in its split: a kind and a number.
#[component]
fn SizeControl(path: Path) -> impl IntoView {
    let state = expect_context::<State>();
    let path = StoredValue::new(path);
    let size = move || {
        state.draft_layout.with(|draft| {
            draft
                .as_ref()
                .and_then(|layout| path.with_value(|path| tree::size(&layout.root, path)))
        })
    };
    let set = move |change: &dyn Fn(Size) -> Size| {
        edit(state, |root| {
            if let Some(size) = path.with_value(|path| size_mut(root, path)) {
                *size = change(*size);
            }
        });
    };
    let kinds = ["Length", "Percentage", "Fill", "Min"].map(|kind| {
        view! {
            <option value=kind selected=move || size().is_some_and(|size| size_parts(size).0 == kind)>
                {kind}
            </option>
        }
    });
    view! {
        <label>
            "size"
            <span class="pair">
                <select on:change=move |event| {
                    let kind = event_target_value(&event);
                    set(&|size| size_of(&kind, size_parts(size).1));
                }>{kinds}</select>
                <input
                    type="number"
                    min="1"
                    prop:value=move || size().map(|size| size_parts(size).1.to_string())
                    on:input=move |event| {
                        if let Ok(value) = event_target_value(&event).parse::<u16>() {
                            set(&|size| size_of(size_parts(size).0, value.max(1)));
                        }
                    }
                />
            </span>
        </label>
    }
}

/// A size's kind and number.
fn size_parts(size: Size) -> (&'static str, u16) {
    match size {
        Size::Length(value) => ("Length", value),
        Size::Percentage(value) => ("Percentage", value),
        Size::Fill(value) => ("Fill", value),
        Size::Min(value) => ("Min", value),
    }
}

/// The size of kind `kind` with number `value`.
fn size_of(kind: &str, value: u16) -> Size {
    match kind {
        "Length" => Size::Length(value),
        "Percentage" => Size::Percentage(value.min(100)),
        "Min" => Size::Min(value),
        _ => Size::Fill(value),
    }
}

/// Which way the picked split runs.
#[component]
fn DirectionControl(path: Path) -> impl IntoView {
    let state = expect_context::<State>();
    let path = StoredValue::new(path);
    let running = move || {
        state.draft_layout.with(|draft| {
            draft.as_ref().and_then(
                |layout| match path.with_value(|path| at(&layout.root, path)) {
                    Some(Pane::Split(direction, _)) => Some(*direction),
                    _ => None,
                },
            )
        })
    };
    let choose = move |event: leptos::ev::Event| {
        let direction = match event_target_value(&event).as_str() {
            "Horizontal" => Direction::Horizontal,
            _ => Direction::Vertical,
        };
        edit(state, |root| {
            if let Some(Pane::Split(running, _)) = path.with_value(|path| at_mut(root, path)) {
                *running = direction;
            }
        });
    };
    view! {
        <label>
            "panes"
            <select on:change=choose>
                <option value="Horizontal" selected=move || running() == Some(Direction::Horizontal)>
                    "side by side"
                </option>
                <option value="Vertical" selected=move || running() == Some(Direction::Vertical)>
                    "stacked"
                </option>
            </select>
        </label>
    }
}

/// What the picked panel shows, and its options.
#[component]
fn PanelControl(
    path: Path,
    pane: impl Fn() -> Option<Pane> + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let state = expect_context::<State>();
    let path = StoredValue::new(path);
    let replace = move |new: Pane| {
        edit(state, |root| {
            if let Some(pane) = path.with_value(|path| at_mut(root, path)) {
                *pane = new;
            }
        });
    };
    let choose = move |event: leptos::ev::Event| {
        let chosen = event_target_value(&event);
        if let Some(new) = PANELS.iter().find(|panel| kind(panel) == chosen) {
            replace(new.clone());
        }
    };
    let kinds = PANELS
        .iter()
        .map(|panel| {
            let name = kind(panel);
            view! {
                <option value=name selected=move || pane().is_some_and(|pane| kind(&pane) == name)>
                    {name}
                </option>
            }
        })
        .collect_view();
    let options = move || {
        match pane() {
        Some(Pane::Die { group }) => view! {
            <label>
                "group by"
                <select on:change=move |event| {
                    let group = match event_target_value(&event).as_str() {
                        "partition" => Grouping::Partition,
                        _ => Grouping::Hardware,
                    };
                    replace(Pane::Die { group });
                }>
                    <option value="hardware" selected=group == Grouping::Hardware>"hardware"</option>
                    <option value="partition" selected=group == Grouping::Partition>"partition"</option>
                </select>
            </label>
        }
        .into_any(),
        Some(Pane::Queue { by }) => view! {
            <label>
                "one row per"
                <select on:change=move |event| {
                    let by = match event_target_value(&event).as_str() {
                        "user" => QueueBy::Login,
                        _ => QueueBy::Partition,
                    };
                    replace(Pane::Queue { by });
                }>
                    <option value="partition" selected=by == QueueBy::Partition>"partition"</option>
                    <option value="user" selected=by == QueueBy::Login>"user"</option>
                </select>
            </label>
        }
        .into_any(),
        Some(Pane::History { span }) => view! { <SpanControl span=span replace=replace/> }.into_any(),
        _ => ().into_any(),
    }
    };
    view! {
        <label>
            "panel"
            <select on:change=choose>{kinds}</select>
        </label>
        {options}
    }
}

/// How far back a History panel looks.
#[component]
fn SpanControl(span: Span, replace: impl Fn(Pane) + Copy + Send + Sync + 'static) -> impl IntoView {
    let (count, days) = match span {
        Span::Hours(hours) => (hours, false),
        Span::Days(days) => (days, true),
    };
    let set = move |count: u32, days: bool| {
        let span = if days {
            Span::Days(count)
        } else {
            Span::Hours(count)
        };
        replace(Pane::History { span });
    };
    view! {
        <label>
            "span"
            <span class="pair">
                <input
                    type="number"
                    min="1"
                    value=count.to_string()
                    on:change=move |event| {
                        if let Ok(count) = event_target_value(&event).parse::<u32>() {
                            set(count.max(1), days);
                        }
                    }
                />
                <select on:change=move |event| set(count, event_target_value(&event) == "days")>
                    <option value="hours" selected=!days>"hours"</option>
                    <option value="days" selected=days>"days"</option>
                </select>
            </span>
        </label>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_panel_has_its_own_kind() {
        let mut kinds: Vec<_> = PANELS.iter().map(kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), PANELS.len());
        assert!(!kinds.contains(&"Split"));
    }

    #[test]
    fn sizes_come_apart_and_back() {
        for size in [
            Size::Length(2),
            Size::Percentage(70),
            Size::Fill(1),
            Size::Min(10),
        ] {
            let (kind, value) = size_parts(size);
            assert_eq!(size_of(kind, value), size);
        }
        assert_eq!(size_of("Percentage", 250), Size::Percentage(100));
    }

    #[test]
    fn labels_say_the_options() {
        assert_eq!(
            label(&Pane::History {
                span: Span::Days(7)
            }),
            "History, 7 d"
        );
        assert_eq!(label(&Pane::Queue { by: QueueBy::Login }), "Queue by user");
        assert_eq!(
            label(&Pane::Split(
                Direction::Horizontal,
                vec![(Size::Fill(1), Pane::Users)]
            )),
            "1 side by side"
        );
    }
}
