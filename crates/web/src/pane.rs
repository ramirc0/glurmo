//! A layout's split tree as nested flexbox. Each panel notes when the pointer enters it, so `f` can fill
//! the stage with it. While the editor is open, each panel wears a cover to pick and drag it by, and
//! each gap between panes a grip to resize them by.

use glurmo_model::layout::{Direction, Pane, Size};
use leptos::prelude::*;

use crate::builder::{Cover, Grip, label};
use crate::history::History;
use crate::jobs::Jobs;
use crate::panels::{Groups, Header, Keys, Mine, Nodes, Partitions, Queue, Ticker, Users};
use crate::state::State;
use crate::tiles::Tiles;
use crate::tree::Path;

/// The view of `pane`, at `path` in its layout, and everything inside it.
pub fn pane(pane: Pane, path: Path, editing: bool) -> AnyView {
    let Pane::Split(direction, children) = pane else {
        let state = expect_context::<State>();
        let cover = editing.then(|| view! { <Cover tag=label(&pane) path=path.clone()/> });
        return view! {
            <div class="leaf" on:pointerenter=move |_| state.pointed.set_value(Some(path.clone()))>
                {panel(&pane)}
                {cover}
            </div>
        }
        .into_any();
    };
    let class = match direction {
        Direction::Vertical => "split column",
        Direction::Horizontal => "split row",
    };
    let last = children.len().saturating_sub(1);
    let slots = children
        .into_iter()
        .enumerate()
        .map(|(index, (size, child))| {
            let grip = (editing && index < last)
                .then(|| view! { <Grip split=path.clone() gap=index direction=direction/> });
            view! {
                <div class="slot" style=flex(size, direction)>
                    {self::pane(child, path.child(index), editing)}
                    {grip}
                </div>
            }
        })
        .collect_view();
    view! { <div class=class>{slots}</div> }.into_any()
}

/// The panel `pane` names. A split is not a panel, so it draws nothing.
fn panel(pane: &Pane) -> AnyView {
    match *pane {
        Pane::Split(..) => ().into_any(),
        Pane::Header => view! { <Header/> }.into_any(),
        Pane::Keys => view! { <Keys/> }.into_any(),
        Pane::Tiles { group } => view! { <Tiles group=group/> }.into_any(),
        Pane::Users => view! { <Users/> }.into_any(),
        Pane::Groups => view! { <Groups/> }.into_any(),
        Pane::Queue { by } => view! { <Queue by=by/> }.into_any(),
        Pane::Partitions => view! { <Partitions/> }.into_any(),
        Pane::Nodes => view! { <Nodes/> }.into_any(),
        Pane::History { span } => view! { <History span=span/> }.into_any(),
        Pane::Mine => view! { <Mine/> }.into_any(),
        Pane::Jobs => view! { <Jobs/> }.into_any(),
        Pane::Ticker => view! { <Ticker/> }.into_any(),
    }
}

/// A slot's flex rule. A length counts lines of text down a column, inside a panel's padding, or
/// characters across a row. Lengths shrink in proportion when a split cannot fit them, and their panels
/// scroll.
fn flex(size: Size, direction: Direction) -> String {
    let span = |length: u16| match direction {
        Direction::Vertical => format!("calc({length} * var(--line) + 2 * var(--pad))"),
        Direction::Horizontal => format!("calc({length} * 1ch)"),
    };
    match size {
        Size::Length(length) => format!("flex: 0 1 {}", span(length)),
        Size::Percentage(share) => format!("flex: 0 0 {share}%"),
        Size::Fill(weight) => format!("flex: {weight} 1 0"),
        Size::Min(length) => format!("flex: 1 1 {}", span(length)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_map_onto_flex_rules() {
        assert_eq!(
            flex(Size::Length(2), Direction::Vertical),
            "flex: 0 1 calc(2 * var(--line) + 2 * var(--pad))"
        );
        assert_eq!(
            flex(Size::Length(30), Direction::Horizontal),
            "flex: 0 1 calc(30 * 1ch)"
        );
        assert_eq!(
            flex(Size::Percentage(70), Direction::Horizontal),
            "flex: 0 0 70%"
        );
        assert_eq!(flex(Size::Fill(2), Direction::Vertical), "flex: 2 1 0");
    }
}
