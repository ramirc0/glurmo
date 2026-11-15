//! Each partition's limit, cores held and free, and jobs running and waiting.

use std::collections::{HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::units::{span, thousands};
use super::view::{Focus, Scene, scrolled, usable};
use crate::model::PartitionName;

/// The widest partition name shown.
const NAME: usize = 12;

/// One row per partition, in the scheduler's order. The default partition is starred.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let cluster = scene.cluster();
    let mut running: HashMap<&PartitionName, u64> = HashMap::new();
    for attempt in cluster.running.values().map(|running| &running.attempt) {
        if let Some(partition) = attempt.partitions.first() {
            *running.entry(partition).or_default() += 1;
        }
    }
    let mut pending: HashMap<&PartitionName, u64> = HashMap::new();
    for attempt in cluster.queue.values() {
        if let Some(partition) = attempt.partitions.first() {
            *pending.entry(partition).or_default() +=
                u64::from(attempt.array.as_ref().map_or(1, |array| array.count));
        }
    }
    let lines = cluster
        .partitions
        .iter()
        .filter(|partition| scene.shows(&[&partition.name.0]))
        .map(|partition| {
            let members: HashSet<_> = partition.nodes.iter().collect();
            let (mut free_cores, mut free_gpus) = (0, 0);
            for node in cluster
                .nodes
                .iter()
                .filter(|node| members.contains(&node.name) && usable(node))
            {
                if let Some(occupancy) = scene.occupancy.get(&node.name) {
                    free_cores +=
                        occupancy.cores.iter().filter(|cell| cell.is_none()).count() as u64;
                    free_gpus += occupancy.gpus.iter().filter(|cell| cell.is_none()).count() as u64;
                }
            }
            let star = if partition.default { "*" } else { "" };
            let name = format!("{}{star}", partition.name.0);
            let limit = partition
                .max_time_s
                .map_or_else(|| "none".to_owned(), |s| span(s.cast_signed()));
            let gpus = if free_gpus > 0 {
                format!("{free_gpus}g")
            } else {
                String::new()
            };
            Line::from(vec![
                Span::styled(
                    format!("{:<NAME$}", name.chars().take(NAME).collect::<String>()),
                    palette.text,
                ),
                Span::styled(
                    format!(
                        "{limit:>6}{:>6}{:>8}{:>8}{:>6}",
                        thousands(running.get(&partition.name).copied().unwrap_or(0)),
                        thousands(pending.get(&partition.name).copied().unwrap_or(0)),
                        format!("{}c", thousands(free_cores)),
                        gpus,
                    ),
                    palette.text,
                ),
            ])
        })
        .collect();
    let head = Line::from(Span::styled(
        format!(
            "{:<NAME$}{:>6}{:>6}{:>8}{:>8}{:>6}",
            "partition", "limit", "run", "pend", "free", ""
        ),
        Style::new()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    ));
    scrolled(
        scene.block("partitions", "", focus),
        Some(head),
        lines,
        area,
        buf,
        focus,
    )
}
