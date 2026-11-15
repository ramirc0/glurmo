//! A layout's split tree as nested flexbox.

use leptos::prelude::*;
use rtviz_model::layout::{Direction, Pane, Size};

use crate::die::Die;
use crate::history::History;
use crate::panels::{Header, Keys, Mine, Nodes, Partitions, Queue, Ticker, Users};

/// The view of `pane` and everything inside it.
pub fn pane(pane: Pane) -> AnyView {
    match pane {
        Pane::Split(direction, children) => {
            let class = match direction {
                Direction::Vertical => "split column",
                Direction::Horizontal => "split row",
            };
            let slots = children
                .into_iter()
                .map(|(size, child)| {
                    view! { <div class="slot" style=flex(size, direction)>{self::pane(child)}</div> }
                })
                .collect_view();
            view! { <div class=class>{slots}</div> }.into_any()
        }
        Pane::Header => view! { <Header/> }.into_any(),
        Pane::Keys => view! { <Keys/> }.into_any(),
        Pane::Die { group } => view! { <Die group=group/> }.into_any(),
        Pane::Users => view! { <Users/> }.into_any(),
        Pane::Queue { by } => view! { <Queue by=by/> }.into_any(),
        Pane::Partitions => view! { <Partitions/> }.into_any(),
        Pane::Nodes => view! { <Nodes/> }.into_any(),
        Pane::History { span } => view! { <History span=span/> }.into_any(),
        Pane::Mine => view! { <Mine/> }.into_any(),
        Pane::Ticker => view! { <Ticker/> }.into_any(),
    }
}

/// A slot's flex rule. A length counts lines of text down a column, or characters across a row.
fn flex(size: Size, direction: Direction) -> String {
    let unit = match direction {
        Direction::Vertical => "var(--line)",
        Direction::Horizontal => "1ch",
    };
    match size {
        Size::Length(length) => format!("flex: 0 0 calc({length} * {unit})"),
        Size::Percentage(share) => format!("flex: 0 0 {share}%"),
        Size::Fill(weight) => format!("flex: {weight} 1 0"),
        Size::Min(length) => format!("flex: 1 1 calc({length} * {unit})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_map_onto_flex_rules() {
        assert_eq!(
            flex(Size::Length(2), Direction::Vertical),
            "flex: 0 0 calc(2 * var(--line))"
        );
        assert_eq!(
            flex(Size::Length(30), Direction::Horizontal),
            "flex: 0 0 calc(30 * 1ch)"
        );
        assert_eq!(
            flex(Size::Percentage(70), Direction::Horizontal),
            "flex: 0 0 70%"
        );
        assert_eq!(flex(Size::Fill(2), Direction::Vertical), "flex: 2 1 0");
    }
}
