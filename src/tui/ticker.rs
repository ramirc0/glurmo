//! Starts and ends as they happen, newest first. It shows no outcomes.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::view::{Focus, Scene, scrolled};
use crate::clock::clock;

/// One line per start or end: the time, which it was, the owner, and the cores and partition.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let lines = scene
        .app
        .ticker
        .iter()
        .filter(|tick| {
            let partition = tick.partition.as_ref().map_or("", |p| p.0.as_str());
            scene.shows(&[&tick.login.0, scene.name(&tick.login), partition])
        })
        .map(|tick| {
            let clock = clock(tick.at, &scene.app.tz, "%H:%M:%S");
            let (mark, color) = if tick.started {
                ("▶ start", palette.text)
            } else {
                ("■ end  ", palette.muted)
            };
            let gpus = if tick.gpus > 0 {
                format!(" {} gpu", tick.gpus)
            } else {
                String::new()
            };
            Line::from(vec![
                Span::styled(format!("{clock} "), palette.muted),
                Span::styled(format!("{mark} "), color),
                Span::styled(
                    format!("{:<12}", scene.name(&tick.login)),
                    scene.color(&tick.login),
                ),
                Span::styled(format!("{:>4} cpu{gpus}  ", tick.cpus), palette.text),
                Span::styled(
                    tick.partition
                        .as_ref()
                        .map_or_else(String::new, |p| p.0.clone()),
                    palette.subtle,
                ),
            ])
        })
        .collect();
    scrolled(
        scene.block("ticker", "", focus),
        None,
        lines,
        area,
        buf,
        focus,
    )
}
