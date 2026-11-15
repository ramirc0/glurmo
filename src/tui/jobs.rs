//! Running and pending jobs, grouped, narrowed by the view's filter. A wide panel adds full names, time
//! left and submit times.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::units::clock;
use super::view::{Focus, Scene, scrolled};
use rtviz_model::jobs::{JobGroup, Order, Phase, Search, job_groups};
use rtviz_model::units::{count, gigabytes, since, span, thousands};

/// One line per group of jobs: owner, IDs and count, name, partitions, CPUs, GPUs, memory, time so far,
/// and nodes or reason. Narrow panels drop the name and partitions, and wide ones add full names, time
/// left and submit times.
pub fn draw(scene: &Scene, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let palette = &scene.palette;
    let columns = Columns::of(area.width);
    let search = Search {
        text: scene.app.filter().unwrap_or_default(),
        ..Search::default()
    };
    let groups = job_groups(scene.cluster(), &search, Order::Cpus);
    let at = scene.now;
    let dash = || "–".to_owned();
    let lines: Vec<Line> = groups
        .iter()
        .map(|group| {
            let first = group.first();
            let running = group.key.phase == Phase::Running;
            let tasks = if group.attempts.len() > 1 {
                format!(" ×{}", group.tasks())
            } else {
                String::new()
            };
            let row = Row {
                who: if columns.wide {
                    scene.name(&first.login).to_owned()
                } else {
                    first.login.0.clone()
                },
                job: format!("{}{tasks}", group.label()),
                name: first.name.clone(),
                partitions: group.key.partitions.clone(),
                cpus: thousands(group.cpus()),
                gpus: count(group.gpus()),
                mem: group.mem_mb().map_or_else(dash, gigabytes),
                time: group.since().map_or_else(dash, |then| since(then, at)),
                left: group.left_s(at).map_or_else(dash, span),
                submitted: group.submitted().map_or_else(String::new, |submit| {
                    clock(submit, &scene.app.tz, "%d %b %H:%M")
                }),
                place: group.place(),
            };
            let mut spans = vec![Span::styled(
                if running { "■ " } else { "□ " },
                scene.color(&first.login),
            )];
            spans.extend(row.spans(columns, palette.text.into(), palette.subtle.into()));
            Line::from(spans)
        })
        .collect();
    let muted = Style::new()
        .fg(palette.muted)
        .add_modifier(Modifier::ITALIC);
    let mut head = vec![Span::raw("  ")];
    head.extend(Row::head().spans(columns, muted, muted));
    let tasks = |which: Phase| -> u64 {
        groups
            .iter()
            .filter(|group| group.key.phase == which)
            .map(JobGroup::tasks)
            .sum()
    };
    let note = format!(
        "{} running, {} pending",
        thousands(tasks(Phase::Running)),
        thousands(tasks(Phase::Pending))
    );
    scrolled(
        scene.block("jobs", &note, focus),
        Some(Line::from(head)),
        lines,
        area,
        buf,
        focus,
    )
}

/// Which optional columns fit.
#[derive(Clone, Copy)]
struct Columns {
    /// The job name and partitions, from 100 columns.
    named: bool,
    /// Full names, time left and submit times, from 150 columns.
    wide: bool,
}

impl Columns {
    fn of(width: u16) -> Self {
        Self {
            named: width >= 100,
            wide: width >= 150,
        }
    }
}

/// A row's text, column by column.
struct Row {
    who: String,
    job: String,
    name: String,
    partitions: String,
    cpus: String,
    gpus: String,
    mem: String,
    time: String,
    left: String,
    submitted: String,
    place: String,
}

impl Row {
    fn head() -> Self {
        let text = |text: &str| text.to_owned();
        Self {
            who: text("user"),
            job: text("job"),
            name: text("name"),
            partitions: text("partition"),
            cpus: text("cpus"),
            gpus: text("gpus"),
            mem: text("mem"),
            time: text("time"),
            left: text("left"),
            submitted: text("submitted"),
            place: text("where or why"),
        }
    }

    /// The cells that fit, padded to their columns: the job IDs and the place in `subtle`, the rest in
    /// `text`.
    fn spans(self, columns: Columns, text: Style, subtle: Style) -> Vec<Span<'static>> {
        let who = if columns.wide { 16 } else { 10 };
        let mut spans = vec![
            Span::styled(format!("{:<who$} ", cut(&self.who, who)), text),
            Span::styled(format!("{:<22} ", cut(&self.job, 22)), subtle),
        ];
        if columns.named {
            spans.push(Span::styled(
                format!(
                    "{:<16} {:<10}",
                    cut(&self.name, 16),
                    cut(&self.partitions, 10)
                ),
                text,
            ));
        }
        spans.push(Span::styled(
            format!(
                "{:>6}{:>5}{:>7}{:>8}  ",
                self.cpus, self.gpus, self.mem, self.time
            ),
            text,
        ));
        if columns.wide {
            spans.push(Span::styled(
                format!("{:>8}  {:<12}  ", self.left, self.submitted),
                text,
            ));
        }
        spans.push(Span::styled(self.place, subtle));
        spans
    }
}

/// The first `width` characters of `text`, ending in `…` when cut.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else {
        let mut kept: String = text.chars().take(width - 1).collect();
        kept.push('…');
        kept
    }
}
