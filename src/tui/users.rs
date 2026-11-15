//! Who holds how many cores and GPUs, with their running and pending jobs.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use rtviz_model::summary::{Holding, holdings};
use rtviz_model::units::thousands;

/// The widest name shown.
const NAME: usize = 12;

/// Squares in the share column.
const SHARE: usize = 8;

/// One row per user, most cores first, and a row for cores the scheduler hides.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let holdings = holdings(scene.cluster(), &scene.occupancy);
    let hidden = holdings.hidden;
    let rows: Vec<_> = holdings
        .users
        .into_iter()
        .filter(|(login, _)| scene.shows(&[&login.0, scene.name(login)]))
        .collect();

    let capacity = scene.cores().total;
    let row = |swatch: &'static str, color: Color, name: &str, you: bool, held: &Holding| {
        let mut spans = vec![
            Span::styled(swatch, color),
            Span::styled(
                format!(" {:<NAME$}", name.chars().take(NAME).collect::<String>()),
                palette.text,
            ),
            if you {
                Span::styled(" you ", Style::new().fg(palette.base).bg(palette.drain))
            } else {
                Span::raw("     ")
            },
            Span::styled(
                format!(
                    "{:>6}{:>5}{:>5}{:>6}  ",
                    thousands(held.cores),
                    held.gpus,
                    thousands(held.jobs),
                    thousands(held.pending)
                ),
                palette.text,
            ),
        ];
        let filled = (held.cores * SHARE as u64)
            .checked_div(capacity)
            .map_or(0, |filled| usize::try_from(filled).unwrap_or(SHARE))
            .max(usize::from(held.cores > 0))
            .min(SHARE);
        spans.push(Span::styled("■".repeat(filled), color));
        spans.push(Span::styled("■".repeat(SHARE - filled), palette.overlay));
        Line::from(spans)
    };
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(login, held)| {
            row(
                "█",
                scene.color(login),
                scene.name(login),
                **login == scene.app.me,
                held,
            )
        })
        .collect();
    if hidden.cores + hidden.gpus > 0 {
        lines.push(row("░", palette.muted, "hidden", false, &hidden));
    }
    let head = Line::from(Span::styled(
        format!(
            "  {:<NAME$}     {:>6}{:>5}{:>5}{:>6}  share",
            "user", "cores", "gpus", "jobs", "pend"
        ),
        Style::new()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    ));
    let note = format!("{} users", rows.len());
    scrolled(
        scene.block("users", &note, focus),
        Some(head),
        lines,
        area,
        buf,
        focus,
    )
}
