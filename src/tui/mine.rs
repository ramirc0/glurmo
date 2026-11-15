//! Your own jobs: running ones first, newest first, then pending ones.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use glurmo_model::Attempt;
use glurmo_model::units::{since, span};

/// The widest job name shown.
const NAME: usize = 16;

/// One line per job of yours: ID, name, partition, CPUs, GPUs, time so far, and nodes or reason.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let cluster = scene.cluster();
    let me = &scene.app.me;
    let mut running: Vec<&Attempt> = cluster
        .running
        .values()
        .map(|running| &running.attempt)
        .filter(|attempt| attempt.login == *me && scene.shows_attempt(attempt))
        .collect();
    running.sort_by_key(|attempt| std::cmp::Reverse(attempt.start));
    let pending = cluster
        .queue
        .values()
        .filter(|attempt| attempt.login == *me && scene.shows_attempt(attempt));
    let line = |attempt: &Attempt, state: &str, time: String, place: String| {
        let id = attempt.printed_id();
        let limit = attempt.time_limit_s.map_or_else(String::new, |limit| {
            format!("/{}", span(limit.cast_signed()))
        });
        Line::from(vec![
            Span::styled(format!("{id:<14}"), palette.text),
            Span::styled(
                format!(
                    "{:<NAME$} ",
                    attempt.name.chars().take(NAME).collect::<String>()
                ),
                palette.subtle,
            ),
            Span::styled(
                format!(
                    "{:<9}{:>5}{:>4}  ",
                    attempt.partitions.first().map_or("", |p| p.0.as_str()),
                    attempt.cpus,
                    attempt.gpus
                ),
                palette.text,
            ),
            Span::styled(format!("{state:<3}"), palette.muted),
            Span::styled(format!("{:>9}  ", time + &limit), palette.text),
            Span::styled(place, palette.subtle),
        ])
    };
    let mut lines: Vec<Line> = running
        .iter()
        .map(|attempt| {
            let time = attempt
                .start
                .map_or_else(String::new, |start| since(start, scene.now));
            let nodes: Vec<&str> = attempt.nodes.iter().map(|node| node.0.as_str()).collect();
            line(attempt, "R", time, nodes.join(","))
        })
        .collect();
    let running_count = lines.len();
    lines.extend(pending.map(|attempt| {
        line(
            attempt,
            "PD",
            since(attempt.key.submit, scene.now),
            attempt.reason.clone().unwrap_or_default(),
        )
    }));
    let head = Line::from(Span::styled(
        format!(
            "{:<14}{:<NAME$} {:<9}{:>5}{:>4}  {:<3}{:>9}  where or why",
            "job", "name", "partition", "cpus", "gpu", "st", "time"
        ),
        Style::new()
            .fg(palette.muted)
            .add_modifier(Modifier::ITALIC),
    ));
    let note = format!(
        "{running_count} running, {} pending",
        lines.len() - running_count
    );
    scrolled(
        scene.block(&format!("mine · {}", me.0), &note, focus),
        Some(head),
        lines,
        area,
        buf,
        focus,
    )
}
