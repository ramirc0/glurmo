//! Pending jobs summed by partition or by user.

use std::collections::{BTreeMap, HashMap};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::units::{since, thousands};
use super::view::{Focus, Scene, scrolled};
use crate::layout::QueueBy;
use crate::model::UnixTime;

/// The widest key shown.
const KEY: usize = 12;

/// What waits under one key.
#[derive(Default)]
struct Waiting<'a> {
    jobs: u64,
    tasks: u64,
    cpus: u64,
    gpus: u64,
    oldest: Option<UnixTime>,
    reasons: HashMap<&'a str, u64>,
}

/// One row per partition or user, most tasks first, with the longest wait and the commonest reason.
pub fn draw(scene: &Scene, by: QueueBy, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let mut groups: BTreeMap<&str, Waiting> = BTreeMap::new();
    for attempt in scene.cluster().queue.values() {
        if !scene.shows_attempt(attempt) {
            continue;
        }
        let key = match by {
            QueueBy::Partition => attempt.partitions.first().map_or("", |p| p.0.as_str()),
            QueueBy::Login => attempt.login.0.as_str(),
        };
        let tasks = u64::from(attempt.array.as_ref().map_or(1, |array| array.count));
        let waiting = groups.entry(key).or_default();
        waiting.jobs += 1;
        waiting.tasks += tasks;
        waiting.cpus += u64::from(attempt.cpus) * tasks;
        waiting.gpus += u64::from(attempt.gpus) * tasks;
        let submit = attempt.key.submit;
        waiting.oldest = Some(waiting.oldest.map_or(submit, |oldest| oldest.min(submit)));
        *waiting
            .reasons
            .entry(attempt.reason.as_deref().unwrap_or("-"))
            .or_default() += tasks;
    }
    let mut rows: Vec<(&str, Waiting)> = groups.into_iter().collect();
    rows.sort_by_key(|(key, waiting)| (std::cmp::Reverse(waiting.tasks), *key));
    let total: u64 = rows.iter().map(|(_, waiting)| waiting.tasks).sum();
    let lines = rows
        .iter()
        .map(|(key, waiting)| {
            let color = match by {
                QueueBy::Partition => palette.text,
                QueueBy::Login => scene.color(&crate::model::Login((*key).to_owned())),
            };
            let reason = waiting
                .reasons
                .iter()
                .max_by_key(|(reason, tasks)| (**tasks, std::cmp::Reverse(**reason)))
                .map_or("", |(reason, _)| reason);
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
                Span::styled(reason.to_owned(), palette.subtle),
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
