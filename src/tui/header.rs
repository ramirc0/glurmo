//! The one-line header and the key line.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::feed::Link;
use super::units::clock;
use super::view::Scene;
use glurmo_model::UnixTime;
use glurmo_model::units::{percent, since, thousands};

/// Eighth blocks, from empty to full.
const EIGHTHS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// The cluster, the sample time, cores and GPUs held, job counts, and any outage on the right.
pub fn header(scene: &Scene, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let palette = &scene.palette;
    let cluster = scene.cluster();
    let site = scene
        .app
        .site
        .as_ref()
        .map_or("", |site| site.cluster.as_str());
    let when = cluster.at.map_or_else(
        || "waiting for the first sample".to_owned(),
        |at| clock(at, &scene.app.tz, "%a %-d %b %H:%M:%S"),
    );
    let cores = scene.cores();
    let gpus = scene.gpus();
    let pending: u64 = cluster
        .queue
        .values()
        .map(|attempt| u64::from(attempt.tasks()))
        .sum();
    let label = |text: &str| Span::styled(format!("{text} "), palette.muted);
    let value = |text: String| Span::styled(text, palette.text);
    let mut spans = vec![
        Span::styled(
            " glurmo ",
            Style::new()
                .fg(palette.base)
                .bg(palette.user(0))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {site} "),
            Style::new().fg(palette.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {when}   "), palette.subtle),
        label("cores"),
    ];
    spans.extend(bar(
        cores.held,
        cores.total,
        16,
        palette.user(0),
        palette.free,
    ));
    spans.push(value(format!(
        " {}/{} {}%   ",
        thousands(cores.held),
        thousands(cores.total),
        percent(cores.held, cores.total)
    )));
    if gpus.total > 0 {
        spans.push(label("gpus"));
        spans.extend(bar(gpus.held, gpus.total, 8, palette.user(1), palette.free));
        spans.push(value(format!(" {}/{}   ", gpus.held, gpus.total)));
    }
    spans.extend([
        label("running"),
        value(format!("{}   ", thousands(cluster.running.len() as u64))),
        label("pending"),
        value(thousands(pending)),
    ]);
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);

    let trouble = match (&scene.app.link, &cluster.outage) {
        (Link::Lost { since: at, error }, _) => Some(format!(
            "server unreachable for {}: {error}",
            since(*at, UnixTime::now())
        )),
        (Link::Connecting, _) => Some("connecting".to_owned()),
        (Link::Live, Some(outage)) => Some(format!(
            "no answer from Slurm for {}: {}",
            since(outage.since, UnixTime::now()),
            outage.error
        )),
        (Link::Live, None) => None,
    };
    if let Some(trouble) = trouble {
        let text = format!(" {} ", trouble.lines().next().unwrap_or_default());
        let width = u16::try_from(text.chars().count())
            .unwrap_or(u16::MAX)
            .min(area.width);
        let style = Style::new().fg(palette.base).bg(palette.down);
        buf.set_stringn(
            area.right() - width,
            area.y,
            text,
            usize::from(width),
            style,
        );
    }
}

/// A bar `width` cells wide, filled to `part / whole` with eighth-cell precision.
pub fn bar(part: u64, whole: u64, width: u16, fill: Color, track: Color) -> Vec<Span<'static>> {
    let eighths = (part * u64::from(width) * 8)
        .checked_div(whole)
        .unwrap_or(0);
    let eighths = usize::try_from(eighths.min(u64::from(width) * 8)).unwrap_or(0);
    let (cells, eighth) = (eighths / 8, eighths % 8);
    let mut spans = vec![Span::styled("█".repeat(cells), Style::new().fg(fill))];
    let mut drawn = cells;
    if eighth > 0 {
        spans.push(Span::styled(
            EIGHTHS[eighth],
            Style::new().fg(fill).bg(track),
        ));
        drawn += 1;
    }
    spans.push(Span::styled(
        " ".repeat(usize::from(width).saturating_sub(drawn)),
        Style::new().bg(track),
    ));
    spans
}

/// The keys, with the filter, layout and theme on the right. Keys that would run into those are left out.
pub fn keys(scene: &Scene, area: Rect, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let palette = &scene.palette;
    let mut right = Vec::new();
    if let Some(filter) = scene.app.filter() {
        right.push(Span::styled(format!("filter “{filter}”   "), palette.drain));
    }
    right.push(Span::styled(
        format!("{} · {} ", scene.app.layout().name, scene.app.theme().name),
        palette.muted,
    ));
    let right = Line::from(right);
    let right_width = u16::try_from(right.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    buf.set_line(area.right() - right_width, area.y, &right, right_width);

    let room = usize::from(area.width - right_width);
    let mut spans = Vec::new();
    let mut width = 0;
    for (key, what) in [
        ("q", "quit"),
        ("tab", "focus"),
        ("/", "filter"),
        ("m", "mine"),
        ("l", "layout"),
        ("t", "theme"),
        ("?", "help"),
        ("f", "fill"),
    ] {
        let chip = format!(" {key} ");
        let label = format!(" {what}  ");
        width += chip.chars().count() + label.chars().count();
        if width > room {
            break;
        }
        spans.push(Span::styled(
            chip,
            Style::new().fg(palette.text).bg(palette.overlay),
        ));
        spans.push(Span::styled(label, palette.subtle));
    }
    buf.set_line(area.x, area.y, &Line::from(spans), area.width - right_width);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn bars_fill_by_eighths_and_keep_their_width() {
        let (fill, track) = (Color::White, Color::Black);
        assert_eq!(text(&bar(0, 10, 4, fill, track)), "    ");
        assert_eq!(text(&bar(5, 10, 4, fill, track)), "██  ");
        assert_eq!(text(&bar(9, 10, 4, fill, track)), "███▌");
        assert_eq!(text(&bar(10, 10, 4, fill, track)), "████");
        assert_eq!(text(&bar(20, 10, 4, fill, track)), "████");
        assert_eq!(text(&bar(1, 0, 4, fill, track)), "    ");
    }
}
