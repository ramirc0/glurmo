//! Pending jobs summed by partition or by user.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use glurmo_model::layout::QueueBy;
use glurmo_model::summary::waiting;
use glurmo_model::units::{since, thousands};

/// The widest key shown.
const KEY: usize = 12;

/// One row per partition or user, most tasks first, with the longest wait and the commonest reason.
pub fn draw(scene: &Scene, by: QueueBy, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let rows = waiting(
        scene
            .cluster()
            .queue
            .values()
            .filter(|attempt| scene.shows_attempt(attempt)),
        by,
    );
    let total: u64 = rows.iter().map(|waiting| waiting.tasks).sum();
    let lines = rows
        .iter()
        .map(|waiting| {
            let key = waiting.key;
            let color = match by {
                QueueBy::Partition => palette.text,
                QueueBy::Login => scene.color(&glurmo_model::Login(key.to_owned())),
            };
            let wait = waiting
                .oldest
                .map_or_else(String::new, |at| since(at, scene.now));
            Line::from(vec![
                Span::styled(
                    format!("{:<KEY$}", key.chars().take(KEY).collect::<String>()),
                    color,
                ),
                Span::styled(
                    format!(
                        "{:>6}{:>8}{:>8}{:>6}{:>6}  ",
                        thousands(waiting.jobs),
                        thousands(waiting.tasks),
                        thousands(waiting.cpus),
                        thousands(waiting.gpus),
                        wait
                    ),
                    palette.text,
                ),
                Span::styled(waiting.reason.to_owned(), palette.subtle),
            ])
        })
        .collect();
    let first = match by {
        QueueBy::Partition => "partition",
        QueueBy::Login => "user",
    };
    let head = Line::from(Span::styled(
        format!(
            "{first:<KEY$}{:>6}{:>8}{:>8}{:>6}{:>6}  why",
            "jobs", "tasks", "cpus", "gpus", "wait"
        ),
        Style::new()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    ));
    let note = format!("{} tasks", thousands(total));
    scrolled(
        scene.block("queue", &note, focus),
        Some(head),
        lines,
        area,
        buf,
        focus,
    )
}
