//! Cores in use over time, stacked by user, with the queue's depth and a time axis below.

use std::collections::HashMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span as Text};
use ratatui::widgets::{Paragraph, Widget};

use super::view::{Focus, Scene};
use glurmo_model::Login;
use glurmo_model::layout::Span;
use glurmo_model::units::thousands;
use glurmo_model::usage::Column;

/// Lower blocks by eighths, from empty to full.
const LOWER: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Columns for the axis labels on the left.
const LABELS: u16 = 7;

/// Hours between time labels. The axis takes the fewest that keep labels apart.
const LABEL_EVERY_H: [i64; 9] = [1, 2, 3, 6, 12, 24, 48, 168, 336];

/// Draws the stacked cores, the queue line and the time axis over `span`.
pub fn draw(scene: &Scene, span: Span, area: Rect, buf: &mut Buffer) {
    let span_s = span.seconds();
    let label = match span {
        Span::Hours(hours) => format!("{hours} h"),
        Span::Days(days) => format!("{days} d"),
    };
    let (usage, error) = (&scene.app.usage, scene.app.usage_error.as_deref());
    let mut about = format!("{label}, stacked by user · queue in tasks");
    if let Some(error) = error.filter(|_| !usage.steps.is_empty()) {
        about = format!("{about} · not updated: {error}");
    }
    let block = scene.block("cores in use", &about, Focus::default());
    let inner = block.inner(area);
    block.render(area, buf);
    if usage.steps.is_empty() {
        let text = error.map_or_else(
            || "no history yet".to_owned(),
            |error| format!("no history: {error}"),
        );
        return note(scene, &text, inner, buf);
    }
    if inner.height < 3 || inner.width <= LABELS || span_s <= 0 {
        return;
    }
    let from = usage.to.0 - span_s;
    let graph = Rect::new(
        inner.x + LABELS,
        inner.y,
        inner.width - LABELS,
        inner.height - 2,
    );
    let columns = usage.columns(from, span_s, graph.width);
    stack(scene, &columns, graph, buf);
    queue(
        scene,
        &columns,
        Rect::new(inner.x, graph.bottom(), inner.width, 1),
        buf,
    );
    axis(
        scene,
        from,
        span_s,
        Rect::new(graph.x, graph.bottom() + 1, graph.width, 1),
        buf,
    );
}

/// Stacks each column's cores from the bottom, one color per login, on a scale up to the usable cores
/// or the peak, whichever is higher. Each cell takes the color of the login under its middle.
fn stack(scene: &Scene, columns: &[Column], area: Rect, buf: &mut Buffer) {
    let tones = &scene.tones;
    let total = |column: &Column| column.cores.iter().map(|(_, cores)| cores).sum::<u64>();
    let peak = columns.iter().map(total).max().unwrap_or(0);
    let capacity = scene.cores().total.max(peak).max(1);
    let rows = u64::from(area.height);
    let mut colors: HashMap<&str, Color> = HashMap::new();
    for (x, column) in (area.x..).zip(columns) {
        // Heights are scaled by `rows`, so each row spans `capacity` units.
        let height = total(column) * rows;
        for (row, y) in (0..rows).zip((area.y..area.bottom()).rev()) {
            let low = row * capacity;
            if height <= low {
                break;
            }
            let middle = u64::midpoint(low, height.min(low + capacity));
            let mut reached = 0;
            let login = column.cores.iter().find_map(|(login, cores)| {
                reached += cores * rows;
                (reached >= middle).then_some(*login)
            });
            let color = login.map_or(tones.muted, |login| {
                *colors
                    .entry(login)
                    .or_insert_with(|| scene.color(&Login(login.to_owned())))
            });
            let eighths = ((height - low) * 8 + capacity / 2) / capacity;
            let glyph = LOWER[usize::try_from(eighths.clamp(1, 8)).unwrap_or(8)];
            buf.set_string(x, y, glyph, Style::new().fg(color));
        }
    }
    let label = |text: String| {
        Text::styled(
            format!("{text:>width$}", width = usize::from(LABELS - 1)),
            tones.muted,
        )
    };
    let left = area.x - LABELS;
    buf.set_span(left, area.y, &label(thousands(capacity)), LABELS);
    buf.set_span(left, area.bottom() - 1, &label("0".into()), LABELS);
}

/// The queue's depth as one line of lower blocks, scaled to its deepest point.
fn queue(scene: &Scene, columns: &[Column], area: Rect, buf: &mut Buffer) {
    let deepest = columns
        .iter()
        .map(|column| column.waiting)
        .max()
        .unwrap_or(0);
    buf.set_span(
        area.x,
        area.y,
        &Text::styled("queue", scene.tones.muted),
        LABELS,
    );
    for (x, column) in (area.x + LABELS..).zip(columns) {
        if column.waiting > 0 {
            let eighths = (column.waiting * 8 + deepest / 2) / deepest;
            let glyph = LOWER[usize::try_from(eighths.clamp(1, 8)).unwrap_or(8)];
            buf.set_string(x, area.y, glyph, Style::new().fg(scene.tones.drain));
        }
    }
}

/// Time labels on whole hours or days in the view's time zone, as often as they fit.
fn axis(scene: &Scene, from: i64, span_s: i64, area: Rect, buf: &mut Buffer) {
    let columns = i64::from(area.width);
    let every_h = LABEL_EVERY_H
        .into_iter()
        .find(|hours| hours * 3_600 * columns / span_s >= 10)
        .unwrap_or(336);
    let mut free_from = 0;
    for hour in ((from / 3_600 + 1) * 3_600..from + span_s).step_by(3_600) {
        let Ok(time) = jiff::Timestamp::from_second(hour) else {
            continue;
        };
        let local = time.to_zoned(scene.app.tz.clone());
        let due = if every_h < 24 {
            i64::from(local.hour()) % every_h == 0
        } else {
            local.hour() == 0 && i64::from(local.day_of_year()) % (every_h / 24) == 0
        };
        if local.minute() != 0 || !due {
            continue;
        }
        let text = local
            .strftime(if every_h < 24 { "%H:%M" } else { "%-d %b" })
            .to_string();
        let width = i64::try_from(text.chars().count()).unwrap_or(0);
        let start = (hour - from) * columns / span_s - width / 2;
        if start < free_from || start + width > columns {
            continue;
        }
        let x = area.x + u16::try_from(start).unwrap_or(0);
        buf.set_string(x, area.y, &text, scene.tones.muted);
        free_from = start + width + 2;
    }
}

fn note(scene: &Scene, text: &str, area: Rect, buf: &mut Buffer) {
    Paragraph::new(Line::styled(text.to_owned(), scene.tones.muted)).render(area, buf);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use jiff::tz::TimeZone;

    use super::*;
    use crate::config::Tui;
    use crate::library::Library;
    use crate::tui::app::App;
    use crate::tui::feed::Update;
    use glurmo_model::UnixTime;
    use glurmo_model::usage::{Step, Usage};

    fn text(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn a_failed_read_keeps_the_last_graph_and_says_why() {
        let mut app = App::new(
            Library::load(Path::new("/nonexistent")),
            &Tui::default(),
            Login("otter".into()),
            TimeZone::UTC,
        );
        let steps = (0..12)
            .map(|i| Step {
                at: UnixTime(i * 300),
                key: "otter".into(),
                core_s: 300 * 64,
                wait_s: 0,
            })
            .collect();
        app.update(Update::Usage(Ok(Usage {
            from: UnixTime(0),
            to: UnixTime(3_600),
            step_s: 300,
            steps,
        })));
        app.update(Update::Usage(Err("connection refused".into())));
        let area = Rect::new(0, 0, 60, 10);
        let mut buf = Buffer::empty(area);
        draw(&Scene::new(&app), Span::Hours(1), area, &mut buf);
        let text = text(&buf);
        assert!(text.contains('█'), "the graph stays:\n{text}");
        assert!(text.contains("connection refused"), "{text}");
    }
}
