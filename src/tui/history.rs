//! Cores in use over time, stacked by user, with the queue's depth and a time axis below.

use std::collections::{BTreeMap, HashMap};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span as Text};
use ratatui::widgets::{Paragraph, Widget};

use super::feed::Usage;
use super::units::thousands;
use super::view::{Focus, Scene};
use rtviz_model::Login;
use rtviz_model::layout::Span;

/// Lower blocks by eighths, from empty to full.
const LOWER: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Columns for the axis labels on the left.
const LABELS: u16 = 7;

/// Hours between time labels. The axis takes the fewest that keep labels apart.
const LABEL_EVERY_H: [i64; 9] = [1, 2, 3, 6, 12, 24, 48, 168, 336];

/// One column of the graph: cores held by each login, biggest user overall first, and tasks waiting.
#[derive(Default)]
struct Column<'a> {
    cores: Vec<(&'a str, u64)>,
    waiting: u64,
}

/// Draws the stacked cores, the queue line and the time axis over `span`.
pub fn draw(scene: &Scene, span: Span, area: Rect, buf: &mut Buffer) {
    let span_s = span.seconds();
    let label = match span {
        Span::Hours(hours) => format!("{hours} h"),
        Span::Days(days) => format!("{days} d"),
    };
    let block = scene.block(
        "cores in use",
        &format!("{label}, stacked by user · queue in tasks"),
        Focus::default(),
    );
    let inner = block.inner(area);
    block.render(area, buf);
    let usage = match &scene.app.usage {
        Ok(usage) if !usage.steps.is_empty() => usage,
        Ok(_) => return note(scene, "no history yet", inner, buf),
        Err(error) => return note(scene, &format!("no history: {error}"), inner, buf),
    };
    if inner.height < 3 || inner.width <= LABELS {
        return;
    }
    let from = usage.to.0 - span_s;
    let graph = Rect::new(
        inner.x + LABELS,
        inner.y,
        inner.width - LABELS,
        inner.height - 2,
    );
    let columns = columns(usage, from, span_s, graph.width);
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

/// Samples the usage step under the middle of each of `count` columns over `from` to `from + span_s`.
fn columns(usage: &Usage, from: i64, span_s: i64, count: u16) -> Vec<Column<'_>> {
    let step_s = u64::try_from(usage.step_s.max(1)).unwrap_or(1);
    let mut by_time: BTreeMap<i64, Column> = BTreeMap::new();
    let mut totals: HashMap<&str, u64> = HashMap::new();
    for step in &usage.steps {
        let column = by_time.entry(step.at.0).or_default();
        column.cores.push((&step.key, step.core_s / step_s));
        column.waiting += step.wait_s / step_s;
        *totals.entry(&step.key).or_default() += step.core_s;
    }
    let count = i64::from(count);
    (0..count)
        .map(|i| {
            let time = from + (2 * i + 1) * span_s / (2 * count);
            let Some((_, step)) = by_time
                .range(..=time)
                .next_back()
                .filter(|(at, _)| **at + usage.step_s > time)
            else {
                return Column::default();
            };
            let mut cores = step.cores.clone();
            cores.sort_by_key(|(login, _)| (std::cmp::Reverse(totals[login]), *login));
            Column {
                cores,
                waiting: step.waiting,
            }
        })
        .collect()
}

/// Stacks each column's cores from the bottom, one color per login, on a scale up to the usable cores
/// or the peak, whichever is higher. Each cell takes the color of the login under its middle.
fn stack(scene: &Scene, columns: &[Column], area: Rect, buf: &mut Buffer) {
    let palette = &scene.palette;
    let total = |column: &Column| column.cores.iter().map(|(_, cores)| cores).sum::<u64>();
    let peak = columns.iter().map(total).max().unwrap_or(0);
    let capacity = scene.cores().0.max(peak).max(1);
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
            let color = login.map_or(palette.muted, |login| {
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
            palette.muted,
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
        &Text::styled("queue", scene.palette.muted),
        LABELS,
    );
    for (x, column) in (area.x + LABELS..).zip(columns) {
        if column.waiting > 0 {
            let eighths = (column.waiting * 8 + deepest / 2) / deepest;
            let glyph = LOWER[usize::try_from(eighths.clamp(1, 8)).unwrap_or(8)];
            buf.set_string(x, area.y, glyph, Style::new().fg(scene.palette.drain));
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
        buf.set_string(x, area.y, &text, scene.palette.muted);
        free_from = start + width + 2;
    }
}

fn note(scene: &Scene, text: &str, area: Rect, buf: &mut Buffer) {
    Paragraph::new(Line::styled(text.to_owned(), scene.palette.muted)).render(area, buf);
}
