//! Every core and GPU, colored by who holds it, node by node in groups.
//!
//! Each character holds two cores: one from the first half of the node's cores on top, and its match
//! from the second half, faded, below. On a two-socket node that puts socket 0 over socket 1.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use super::view::{Focus, Scene, scrolled};
use glurmo_model::layout::Grouping;
use glurmo_model::summary::{NodeGroup, node_groups};
use glurmo_model::units::{horizon, load, thousands};
use glurmo_model::{Node, NodeFlag, NodeState};

/// Columns between node entries.
const GAP: u16 = 3;

/// How much of its color the lower core in each character keeps.
const LOWER: f32 = 0.68;

/// How much of the down or drain color the free cores of a node that is out keep, so a dead node
/// reads as dim rather than loud.
const OUT: f32 = 0.4;

/// Draws the groups' titles and node entries, as many entries to a line as fit. When they do not fit
/// the panel's height, each node's load and free memory are left out to fit more to a line.
pub fn draw(scene: &Scene, grouping: Grouping, area: Rect, buf: &mut Buffer, focus: Focus) -> u16 {
    let inner = Block::bordered().inner(area);
    let groups = node_groups(scene.cluster(), grouping);
    let mut gauges = true;
    let mut lines = lines_of(scene, &groups, inner.width, gauges);
    if lines.len() > usize::from(inner.height) {
        let compact = lines_of(scene, &groups, inner.width, false);
        if compact.len() < lines.len() {
            gauges = false;
            lines = compact;
        }
    }
    let note = if gauges {
        "host · cores by user · load · free mem"
    } else {
        "host · cores by user"
    };
    let limit = scrolled(
        scene.block("cores", note, focus),
        None,
        lines,
        area,
        buf,
        focus,
    );
    let legend = legend(scene);
    let width = u16::try_from(legend.width()).unwrap_or(u16::MAX);
    if width > 0 && width + 12 < area.width {
        buf.set_line(area.right() - width - 2, area.y, &legend, width);
    }
    limit
}

/// Under a palette by time left, each step's color with the time left at which it ends, then the
/// rest color for jobs with more. Empty under any other palette.
fn legend(scene: &Scene) -> Line<'static> {
    let mut spans = Vec::new();
    let mut last = 0;
    for (end_s, color) in scene.coloring.steps() {
        spans.push(Span::styled(" ■", scene.painted(Some(color))));
        spans.push(Span::styled(horizon(end_s), scene.tones.muted));
        last = end_s;
    }
    if last > 0 {
        spans.push(Span::styled(" ■", scene.painted(None)));
        spans.push(Span::styled(
            format!("{}+ ", horizon(last)),
            scene.tones.muted,
        ));
    }
    Line::from(spans)
}

/// Each group's title, then its entries in lines `width` wide. A group whose entries are wider than
/// that puts one node on each line and wraps its cores, GPUs and gauges onto lines below.
fn lines_of(scene: &Scene, groups: &[NodeGroup], width: u16, gauges: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for group in groups {
        lines.push(title(scene, group));
        let shape = Shape::of(&group.nodes, gauges);
        if shape.width() > width {
            for node in &group.nodes {
                lines.extend(entry(scene, node, &shape).wrapped(usize::from(width)));
            }
            continue;
        }
        let per_line = usize::from((width + GAP) / (shape.width() + GAP));
        for row in group.nodes.chunks(per_line) {
            let mut spans = Vec::new();
            for (i, node) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" ".repeat(usize::from(GAP))));
                }
                spans.extend(entry(scene, node, &shape).spans());
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// The group's name, its node count, and the cores and GPUs held.
fn title(scene: &Scene, group: &NodeGroup) -> Line<'static> {
    let tones = &scene.tones;
    let cores = group.cores(&scene.occupancy);
    let gpus = group.gpus(&scene.occupancy);
    let count = group.nodes.len();
    let mut spans = vec![
        Span::styled(
            group.name.clone(),
            Style::new().fg(tones.user(0)).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {count} node{}", if count == 1 { "" } else { "s" }),
            tones.subtle,
        ),
        Span::styled(
            format!(
                "  {}/{} cores held",
                thousands(cores.held),
                thousands(cores.total)
            ),
            tones.muted,
        ),
    ];
    if gpus.total > 0 {
        spans.push(Span::styled(
            format!("  {}/{} gpus held", gpus.held, gpus.total),
            tones.muted,
        ));
    }
    Line::from(spans)
}

/// The widths every entry in a group shares, so their columns line up, and whether entries show
/// load and free memory.
struct Shape {
    name: u16,
    strip: u16,
    gpus: u16,
    gauges: bool,
}

impl Shape {
    fn of(nodes: &[&Node], gauges: bool) -> Self {
        let most = |width: fn(&Node) -> usize| {
            u16::try_from(nodes.iter().map(|node| width(node)).max().unwrap_or(0))
                .unwrap_or(u16::MAX)
        };
        Self {
            name: most(|node| node.name.0.chars().count()),
            strip: most(|node| node.cores.div_ceil(2) as usize),
            gpus: most(|node| node.gpus as usize),
            gauges,
        }
    }

    /// The entry's width: name, strip, GPUs, then load and free memory, with a space between each.
    fn width(&self) -> u16 {
        let gpus = if self.gpus > 0 { self.gpus + 1 } else { 0 };
        let gauges = if self.gauges { 6 + 5 } else { 0 };
        self.name + 1 + self.strip + gpus + gauges
    }
}

/// One node: its name, then pieces a line may break between. They are its cores, its GPUs, and its
/// load and free memory or why it is out.
struct Entry {
    name: Span<'static>,
    pieces: Vec<Vec<Span<'static>>>,
}

impl Entry {
    /// The entry on one line, a space between its parts.
    fn spans(self) -> Vec<Span<'static>> {
        let mut spans = vec![self.name];
        for piece in self.pieces {
            spans.push(Span::raw(" "));
            spans.extend(piece);
        }
        spans
    }

    /// The entry on as many lines `width` wide as it takes, its pieces lined up after the name.
    fn wrapped(self, width: usize) -> Vec<Line<'static>> {
        let indent = self.name.width() + 1;
        let room = width.saturating_sub(indent).max(1);
        let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut used = 0;
        for piece in self.pieces {
            let piece_width: usize = piece.iter().map(Span::width).sum();
            if used > 0 && used + 1 + piece_width <= room {
                rows.last_mut().unwrap().push(Span::raw(" "));
                used += 1;
            } else if used > 0 {
                rows.push(Vec::new());
                used = 0;
            }
            for span in piece {
                if used > 0 && used + span.width() > room {
                    rows.push(Vec::new());
                    used = 0;
                }
                used += span.width();
                rows.last_mut().unwrap().push(span);
            }
        }
        let mut lead = Some(vec![self.name, Span::raw(" ")]);
        rows.into_iter()
            .map(|row| {
                let mut spans = lead
                    .take()
                    .unwrap_or_else(|| vec![Span::raw(" ".repeat(indent))]);
                spans.extend(row);
                Line::from(spans)
            })
            .collect()
    }
}

/// One node's entry, its columns as wide as `shape` says.
fn entry(scene: &Scene, node: &Node, shape: &Shape) -> Entry {
    let tones = &scene.tones;
    let out = out(scene, node);
    let empty = out.map_or(tones.free, |color| tones.fade(color, OUT));
    let name_color = out.filter(|_| !node.is_usable()).unwrap_or(tones.subtle);
    let name = Span::styled(
        format!("{:<width$}", node.name.0, width = usize::from(shape.name)),
        name_color,
    );
    let (cores, gpus) = scene
        .occupancy
        .get(&node.name)
        .map_or((&[][..], &[][..]), |occupancy| {
            (occupancy.cores.as_slice(), occupancy.gpus.as_slice())
        });
    let half = cores.len().div_ceil(2);
    let strip = (0..usize::from(shape.strip))
        .map(|i| {
            if i >= half {
                return Span::raw(" ");
            }
            let top = scene.cell(cores[i], empty);
            let bottom = cores.get(i + half).map_or(tones.base, |cell| {
                tones.fade(scene.cell(*cell, empty), LOWER)
            });
            Span::styled("▀", Style::new().fg(top).bg(bottom))
        })
        .collect();
    let mut pieces = vec![strip];
    if shape.gpus > 0 {
        pieces.push(
            (0..usize::from(shape.gpus))
                .map(|i| match gpus.get(i) {
                    Some(cell) => Span::styled("■", scene.cell(*cell, empty)),
                    None => Span::raw(" "),
                })
                .collect(),
        );
    }
    if shape.gauges {
        pieces.push(gauges(scene, node, out));
    }
    Entry { name, pieces }
}

/// A node's load and free memory, or why it is out.
fn gauges(scene: &Scene, node: &Node, out: Option<Color>) -> Vec<Span<'static>> {
    let tones = &scene.tones;
    if let Some(word) = node.trouble() {
        return vec![Span::styled(
            format!("{word:<10}"),
            out.unwrap_or(tones.text),
        )];
    }
    let load = node.load.map_or_else(|| "-".to_owned(), load);
    let free = node
        .mem_free_mb
        .map(|free| (free * 100).checked_div(node.mem_mb).unwrap_or(0).min(99));
    let free = free.map_or_else(|| "-".to_owned(), |free| format!("{free}%"));
    vec![
        Span::styled(format!("{load:>5}"), tones.text),
        Span::styled(format!(" {free:>4}"), tones.muted),
    ]
}

/// The color of a node that is out: down or draining. `None` for a node that takes jobs.
fn out(scene: &Scene, node: &Node) -> Option<Color> {
    match node.state {
        _ if node.flags.contains(&NodeFlag::NotResponding) => Some(scene.tones.down),
        NodeState::Down | NodeState::Unknown => Some(scene.tones.down),
        NodeState::Draining | NodeState::Drained => Some(scene.tones.drain),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::view::tests::busy_app;

    #[tokio::test]
    async fn nodes_wider_than_the_panel_wrap_and_keep_every_gpu() {
        let app = busy_app().await;
        let scene = Scene::new(&app);
        for grouping in [Grouping::Hardware, Grouping::Partition] {
            let groups = node_groups(scene.cluster(), grouping);
            let gpus: u32 = groups
                .iter()
                .flat_map(|group| &group.nodes)
                .map(|node| node.gpus)
                .sum();
            for gauges in [true, false] {
                let lines = lines_of(&scene, &groups, 78, gauges);
                assert!(lines.iter().all(|line| line.width() <= 78), "{grouping:?}");
                let drawn = lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .filter(|span| span.content == "■")
                    .count();
                assert_eq!(drawn, gpus as usize, "{grouping:?} with gauges {gauges}");
            }
        }
    }
}
