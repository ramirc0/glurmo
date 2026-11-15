//! Each partition's limit, cores held and free, and jobs running and waiting.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use rtviz_model::summary::partition_loads;
use rtviz_model::units::{span, thousands};

/// The widest partition name shown.
const NAME: usize = 12;

/// One row per partition, in the scheduler's order. The default partition is starred.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let cluster = scene.cluster();
    let lines = partition_loads(cluster, &scene.occupancy)
        .iter()
        .filter(|load| scene.shows(&[&load.partition.name.0]))
        .map(|load| {
            let partition = load.partition;
            let star = if partition.default { "*" } else { "" };
            let name = format!("{}{star}", partition.name.0);
            let limit = partition
                .max_time_s
                .map_or_else(|| "none".to_owned(), |s| span(s.cast_signed()));
            let gpus = if load.free_gpus > 0 {
                format!("{}g", load.free_gpus)
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
                        thousands(load.running),
                        thousands(load.pending),
                        format!("{}c", thousands(load.free_cores)),
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
