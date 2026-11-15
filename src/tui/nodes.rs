//! Node health: how many nodes are in each state, then each node that cannot take jobs and why.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled, trouble};
use rtviz_model::NodeState;

/// Counts by state on the first line, then one line per node that is out.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let nodes = &scene.cluster().nodes;
    let mut counts = Vec::new();
    for (state, word) in [
        (NodeState::Idle, "idle"),
        (NodeState::Mixed, "mixed"),
        (NodeState::Allocated, "full"),
    ] {
        let count = nodes
            .iter()
            .filter(|node| node.state == state && trouble(node).is_none())
            .count();
        counts.push(Span::styled(format!("{count} "), palette.text));
        counts.push(Span::styled(format!("{word}   "), palette.muted));
    }
    let out: Vec<_> = nodes
        .iter()
        .filter_map(|node| Some((node, trouble(node)?)))
        .filter(|(node, word)| {
            scene.shows(&[&node.name.0, word, node.reason.as_deref().unwrap_or("")])
        })
        .collect();
    counts.push(Span::styled(format!("{} ", out.len()), palette.down));
    counts.push(Span::styled("out", palette.muted));
    let width = out
        .iter()
        .map(|(node, _)| node.name.0.len())
        .max()
        .unwrap_or(0);
    let lines = out
        .iter()
        .map(|(node, word)| {
            let color = if matches!(*word, "draining" | "drained") {
                palette.drain
            } else {
                palette.down
            };
            Line::from(vec![
                Span::styled(format!("{:<width$}  ", node.name.0), palette.text),
                Span::styled(format!("{word:<9}"), color),
                Span::styled(node.reason.clone().unwrap_or_default(), palette.subtle),
            ])
        })
        .collect();
    scrolled(
        scene.block("nodes", "", focus),
        Some(Line::from(counts)),
        lines,
        area,
        buf,
        focus,
    )
}
