//! What each user group's members hold, with their running and pending jobs.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use glurmo_model::summary::group_holdings;
use glurmo_model::units::thousands;

/// The widest name shown.
const NAME: usize = 16;

/// Squares in the share column.
const SHARE: usize = 8;

/// One row per group, most cores first. A user in two groups counts in both.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let capacity = scene.cores().total;
    let lines: Vec<Line> = group_holdings(scene.cluster(), &scene.occupancy)
        .into_iter()
        .filter(|group| scene.shows(&[&group.name.0]))
        .map(|group| {
            let held = group.held;
            let filled = (held.cores * SHARE as u64)
                .checked_div(capacity)
                .map_or(0, |filled| usize::try_from(filled).unwrap_or(SHARE))
                .max(usize::from(held.cores > 0))
                .min(SHARE);
            Line::from(vec![
                Span::styled(
                    format!(
                        "{:<NAME$}{:>6}{:>7}{:>5}{:>5}{:>6}  ",
                        group.name.0.chars().take(NAME).collect::<String>(),
                        group.users,
                        thousands(held.cores),
                        held.gpus,
                        thousands(held.jobs),
                        thousands(held.pending)
                    ),
                    palette.text,
                ),
                Span::styled("■".repeat(filled), palette.subtle),
                Span::styled("■".repeat(SHARE - filled), palette.overlay),
            ])
        })
        .collect();
    let head = Line::from(Span::styled(
        format!(
            "{:<NAME$}{:>6}{:>7}{:>5}{:>5}{:>6}  share",
            "group", "users", "cores", "gpus", "jobs", "pend"
        ),
        Style::new()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    ));
    let note = format!("{} groups", lines.len());
    scrolled(
        scene.block("groups", &note, focus),
        Some(head),
        lines,
        area,
        buf,
        focus,
    )
}
