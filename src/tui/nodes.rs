//! Node health: how many nodes are in each state, then each node that cannot take jobs and why.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use glurmo_model::NodeState;

/// Counts by state on the first line, then one line per node that is out.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let tones = &scene.tones;
    let nodes = &scene.cluster().nodes;
    let mut counts = Vec::new();
    for (state, word) in [
        (NodeState::Idle, "idle"),
        (NodeState::Mixed, "mixed"),
        (NodeState::Allocated, "full"),
    ] {
        let count = nodes
            .iter()
            .filter(|node| node.state == state && node.trouble().is_none())
            .count();
        counts.push(Span::styled(format!("{count} "), tones.text));
        counts.push(Span::styled(format!("{word}   "), tones.muted));
    }
    let out: Vec<_> = nodes
        .iter()
        .filter_map(|node| Some((node, node.trouble()?)))
        .filter(|(node, word)| {
            scene.shows(&[&node.name.0, word, node.reason.as_deref().unwrap_or("")])
        })
        .collect();
    counts.push(Span::styled(format!("{} ", out.len()), tones.down));
    counts.push(Span::styled("out", tones.muted));
    let width = out
        .iter()
        .map(|(node, _)| node.name.0.len())
        .max()
        .unwrap_or(0);
    let lines = out
        .iter()
        .map(|(node, word)| {
            let color = if matches!(*word, "draining" | "drained") {
                tones.drain
            } else {
                tones.down
            };
            Line::from(vec![
                Span::styled(format!("{:<width$}  ", node.name.0), tones.text),
                Span::styled(format!("{word:<9}"), color),
                Span::styled(node.reason.clone().unwrap_or_default(), tones.subtle),
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
