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
use rtviz_model::layout::Grouping;
use rtviz_model::summary::{NodeGroup, node_groups};
use rtviz_model::units::{load, thousands};
use rtviz_model::{Node, NodeFlag, NodeState};

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
    scrolled(
        scene.block("cores", note, focus),
        None,
        lines,
        area,
        buf,
        focus,
    )
}

/// Each group's title, then its entries in lines `width` wide.
fn lines_of(scene: &Scene, groups: &[NodeGroup], width: u16, gauges: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for group in groups {
        lines.push(title(scene, group));
        let shape = Shape::of(&group.nodes, gauges);
        let per_line = usize::from(((width + GAP) / (shape.width() + GAP)).max(1));
        for row in group.nodes.chunks(per_line) {
            let mut spans = Vec::new();
            for (i, node) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" ".repeat(usize::from(GAP))));
                }
                spans.extend(entry(scene, node, &shape));
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// The group's name, its node count, and the cores and GPUs held.
fn title(scene: &Scene, group: &NodeGroup) -> Line<'static> {
    let palette = &scene.palette;
    let cores = group.cores(&scene.occupancy);
    let gpus = group.gpus(&scene.occupancy);
    let count = group.nodes.len();
    let mut spans = vec![
        Span::styled(
            group.name.clone(),
            Style::new()
                .fg(palette.user(0))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {count} node{}", if count == 1 { "" } else { "s" }),
            palette.subtle,
        ),
        Span::styled(
            format!(
                "  {}/{} cores held",
                thousands(cores.held),
                thousands(cores.total)
            ),
            palette.muted,
        ),
    ];
    if gpus.total > 0 {
        spans.push(Span::styled(
            format!("  {}/{} gpus held", gpus.held, gpus.total),
            palette.muted,
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

/// One node: its name, its cores, its GPUs, then its load and free memory or why it is out.
fn entry(scene: &Scene, node: &Node, shape: &Shape) -> Vec<Span<'static>> {
    let palette = &scene.palette;
    let out = out(scene, node);
    let empty = out.map_or(palette.free, |color| palette.fade(color, OUT));
    let name_color = out.filter(|_| !node.is_usable()).unwrap_or(palette.subtle);
    let mut spans = vec![Span::styled(
        format!("{:<width$} ", node.name.0, width = usize::from(shape.name)),
        name_color,
    )];
    let (cores, gpus) = scene
        .occupancy
        .get(&node.name)
        .map_or((&[][..], &[][..]), |occupancy| {
            (occupancy.cores.as_slice(), occupancy.gpus.as_slice())
        });
    let half = cores.len().div_ceil(2);
    for i in 0..usize::from(shape.strip) {
        if i >= half {
            spans.push(Span::raw(" "));
            continue;
        }
        let top = scene.cell(cores[i], empty);
        let bottom = cores.get(i + half).map_or(palette.base, |cell| {
            palette.fade(scene.cell(*cell, empty), LOWER)
        });
        spans.push(Span::styled("▀", Style::new().fg(top).bg(bottom)));
    }
    if shape.gpus > 0 {
        spans.push(Span::raw(" "));
        for i in 0..usize::from(shape.gpus) {
            match gpus.get(i) {
                Some(cell) => spans.push(Span::styled("■", scene.cell(*cell, empty))),
                None => spans.push(Span::raw(" ")),
            }
        }
    }
    if !shape.gauges {
        return spans;
    }
    if let Some(word) = node.trouble() {
        spans.push(Span::styled(
            format!(" {word:<10}"),
            out.unwrap_or(palette.text),
        ));
        return spans;
    }
    let load = node.load.map_or_else(|| "-".to_owned(), load);
    let free = node
        .mem_free_mb
        .map(|free| (free * 100).checked_div(node.mem_mb).unwrap_or(0).min(99));
    let free = free.map_or_else(|| "-".to_owned(), |free| format!("{free}%"));
    spans.push(Span::styled(format!(" {load:>5}"), palette.text));
    spans.push(Span::styled(format!(" {free:>4}"), palette.muted));
    spans
}

/// The color of a node that is out: down or draining. `None` for a node that takes jobs.
fn out(scene: &Scene, node: &Node) -> Option<Color> {
    match node.state {
        _ if node.flags.contains(&NodeFlag::NotResponding) => Some(scene.palette.down),
        NodeState::Down | NodeState::Unknown => Some(scene.palette.down),
        NodeState::Draining | NodeState::Drained => Some(scene.palette.drain),
        _ => None,
    }
}
