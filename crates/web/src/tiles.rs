//! Every core and GPU, colored by who holds it, in a tile per node, with nodes in groups.
//!
//! Tiles grow to the largest core size at which every group fits the panel, dropping each node's load
//! and memory gauges before shrinking the cores. When even the smallest size does not fit, each node
//! shrinks to a brief tile: its name over one bar of its cores by user, then both on one line.

use std::fmt::Write;

use glurmo_model::layout::Grouping;
use glurmo_model::palette::Coloring;
use glurmo_model::summary::{NodeGroup, node_groups};
use glurmo_model::theme::Theme;
use glurmo_model::units::{gigabytes, horizon, load, thousands};
use glurmo_model::{Cluster, Node, NodeFlag, NodeState, Occupancies, Owner};
use leptos::prelude::*;
use web_sys::CanvasRenderingContext2d;

use crate::paint::{Rgb, Surface, painted};
use crate::replay::{Cell, Effect, Effects};
use crate::state::{Highlight, State};
use crate::tip::Tip;

/// The largest and smallest core pitch tried, in CSS pixels, and the smallest that keeps gauges.
const PITCHES: std::ops::RangeInclusive<u32> = 5..=18;
const GAUGED_PITCHES: std::ops::RangeInclusive<u32> = 8..=18;
/// Space around the board and between tiles.
const MARGIN: f64 = 10.0;
const GAP: f64 = 8.0;
/// Height of a group's title line, and space between rows of groups and groups side by side.
const LABEL: f64 = 24.0;
const SHELF_GAP: f64 = 14.0;
const GROUP_GAP: f64 = 26.0;
/// A full tile's padding, name line, and gauge lines or bare bottom edge.
const PAD: f64 = 8.0;
const HEAD: f64 = 20.0;
const BARE_FOOT: f64 = PAD;
/// Gauge lines: how far their text rises above its baseline, how far a tick hangs below it, and the
/// distance between the two baselines. The lines keep `PAD` from the cores above and the edge below.
const GAUGE_RISE: f64 = 7.5;
const GAUGE_DROP: f64 = 2.5;
const GAUGE_ROW: f64 = 13.0;
const FOOT: f64 = PAD + GAUGE_RISE + GAUGE_ROW + GAUGE_DROP + PAD;
const MIN_TILE_W: f64 = 116.0;
/// The shortest gauge track worth drawing.
const MIN_TRACK: f64 = 16.0;
/// Brief tiles, tried in order: the name over the bar, then the name beside it.
const BRIEFS: [Brief; 2] = [
    Brief {
        w: 132.0,
        h: 34.0,
        one_line: false,
    },
    Brief {
        w: 96.0,
        h: 18.0,
        one_line: true,
    },
];
/// The advance of a brief tile's name characters.
const BRIEF_CHAR: f64 = 6.0;
/// Approximate advances of a title's characters and its note's, to keep titles from running into each
/// other. The note is monospace.
const TITLE_CHAR: f64 = 8.5;
const NOTE_CHAR: f64 = 6.7;
/// How much of its color a faded core keeps when someone else is highlighted.
const FADED: f64 = 0.22;
/// How opaque a node's tile stays when a highlight leaves it out.
const FADED_TILE: f64 = 0.3;
/// How much of the down or drain color the free cores of a node that is out keep.
const OUT: f64 = 0.4;

/// A rectangle in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    /// Whether `(x, y)` lies inside, or within `slop` of an edge.
    fn holds(&self, x: f64, y: f64, slop: f64) -> bool {
        x >= self.x - slop
            && x <= self.x + self.w + slop
            && y >= self.y - slop
            && y <= self.y + self.h + slop
    }
}

/// The part of a tile under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// A core, by index.
    Core(usize),
    /// A GPU, by index.
    Gpu(usize),
    /// Anywhere else on the tile.
    Tile,
}

/// The node whose tile holds `(x, y)` on `board`, and what part of it the point is on. A point in the
/// gap between two cores counts as the nearer one.
pub fn spot_at<'a>(board: &Board<'a>, x: f64, y: f64) -> Option<(&'a Node, Spot)> {
    let tile = board
        .tiles
        .iter()
        .find(|tile| tile.frame.holds(x, y, 0.0))?;
    let near = |rects: &[Rect]| rects.iter().position(|rect| rect.holds(x, y, 1.0));
    let spot = near(&tile.cores)
        .map(Spot::Core)
        .or_else(|| near(&tile.gpus).map(Spot::Gpu))
        .unwrap_or(Spot::Tile);
    Some((tile.node, spot))
}

/// Who holds the core or GPU at `spot` on `node`.
pub fn owner_at(occupancy: &Occupancies, node: &Node, spot: Spot) -> Option<Owner> {
    let cells = occupancy.get(&node.name)?;
    match spot {
        Spot::Core(i) => cells.cores.get(i).copied().flatten(),
        Spot::Gpu(i) => cells.gpus.get(i).copied().flatten(),
        Spot::Tile => None,
    }
}

/// Lights what the pointer is on: the job that holds a core or GPU, else the node. Clears what it lit
/// once the pointer leaves the tiles. Highlights from elsewhere stay until the pointer finds a tile.
fn light_from_tiles(state: State, lit: Option<Highlight>) {
    if state.tv {
        return;
    }
    let from_tiles = state.highlight.with_untracked(|highlight| {
        matches!(highlight, Some(Highlight::Job(_) | Highlight::Node(_)))
    });
    if (lit.is_some() || from_tiles)
        && state
            .highlight
            .with_untracked(|highlight| *highlight != lit)
    {
        state.highlight.set(lit);
    }
}

/// Where one node's tile, cores and GPUs go.
#[derive(Debug, PartialEq)]
pub struct Tile<'a> {
    /// The node.
    pub node: &'a Node,
    /// The whole tile.
    pub frame: Rect,
    /// Where the name's baseline starts.
    pub name: (f64, f64),
    /// Each core, by index. Empty for a brief tile.
    pub cores: Vec<Rect>,
    /// A brief tile's bar of cores by user.
    pub bar: Option<Rect>,
    /// Each GPU, by index.
    pub gpus: Vec<Rect>,
}

/// Where one group's title goes.
#[derive(Debug, PartialEq)]
pub struct Label {
    /// Which group, by index into the groups.
    pub group: usize,
    /// The title's left edge and baseline.
    pub x: f64,
    /// The title's top.
    pub y: f64,
    /// The width of the group's tiles below it.
    pub w: f64,
}

/// Every tile and title, and the height they take.
#[derive(Debug, PartialEq)]
pub struct Board<'a> {
    /// The tiles.
    pub tiles: Vec<Tile<'a>>,
    /// The titles.
    pub labels: Vec<Label>,
    /// The height the board needs.
    pub height: f64,
    /// How the tiles look.
    pub style: Style,
}

/// How tiles look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    /// A grid of cores at a pitch, with or without gauges.
    Full(Pitch),
    /// A bar of cores by user.
    Brief(Brief),
}

/// A tile with no core grid: the node's name and one bar of its cores by user.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brief {
    w: f64,
    h: f64,
    one_line: bool,
}

/// How a node's cores and GPUs arrange: sockets stacked, each a grid of rows.
struct Shape {
    sockets: u32,
    per_socket: u32,
    cols: u32,
    rows: u32,
    gpus: u32,
    gpus_per_row: u32,
}

impl Shape {
    /// A grid about six times wider than tall for each socket, and GPUs four to a row.
    fn of(node: &Node) -> Self {
        let sockets = node.sockets.max(1);
        let per_socket = node.cores.div_ceil(sockets).max(1);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a small positive count"
        )]
        let cols = (f64::from(per_socket) * 6.0).sqrt().ceil() as u32;
        let cols = cols.clamp(1, per_socket);
        Self {
            sockets,
            per_socket,
            cols,
            rows: per_socket.div_ceil(cols),
            gpus: node.gpus,
            gpus_per_row: node.gpus.clamp(1, 4),
        }
    }
}

/// Lengths that follow from the core pitch, and whether tiles have gauges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pitch {
    gauges: bool,
    step: f64,
    gap: f64,
    socket_gap: f64,
    chip: f64,
    chip_gap: f64,
}

impl Pitch {
    fn new(pitch: u32, gauges: bool) -> Self {
        let pitch = f64::from(pitch);
        Self {
            gauges,
            step: pitch,
            gap: if pitch >= 10.0 { 2.0 } else { 1.0 },
            socket_gap: (pitch / 2.0).max(3.0),
            chip: pitch * 2.0,
            chip_gap: (pitch / 2.0).max(2.0),
        }
    }

    fn grid(&self, shape: &Shape) -> (f64, f64) {
        let socket_h = f64::from(shape.rows) * self.step - self.gap;
        let w = f64::from(shape.cols) * self.step - self.gap;
        let sockets = f64::from(shape.sockets);
        (w, sockets * socket_h + (sockets - 1.0) * self.socket_gap)
    }

    fn chips(&self, shape: &Shape) -> (f64, f64) {
        if shape.gpus == 0 {
            return (0.0, 0.0);
        }
        let per_row = f64::from(shape.gpus_per_row);
        let rows = f64::from(shape.gpus.div_ceil(shape.gpus_per_row));
        (
            per_row * self.chip + (per_row - 1.0) * self.chip_gap,
            rows * self.chip + (rows - 1.0) * self.chip_gap,
        )
    }

    /// The body's size: GPUs above the core grid.
    fn body(&self, shape: &Shape) -> (f64, f64) {
        let (grid_w, grid_h) = self.grid(shape);
        let (chips_w, chips_h) = self.chips(shape);
        let between = if shape.gpus > 0 { self.step } else { 0.0 };
        (grid_w.max(chips_w), grid_h + chips_h + between)
    }

    fn tile(&self, node: &Node) -> (f64, f64) {
        let (w, h) = self.body(&Shape::of(node));
        let foot = if self.gauges { FOOT } else { BARE_FOOT };
        ((w + 2.0 * PAD).max(MIN_TILE_W), HEAD + h + foot)
    }
}

/// Lays out `groups` in a panel `width` wide, at the largest core pitch whose board fits `height`. Tiles
/// keep their gauges while cores stay at least the smallest gauged pitch, and drop them after.
/// Tiles go brief when nothing fits, and take the smallest brief style when even that overflows.
pub fn board<'a>(groups: &[NodeGroup<'a>], width: f64, height: f64) -> Board<'a> {
    let with_gauges = GAUGED_PITCHES
        .rev()
        .map(|pitch| Style::Full(Pitch::new(pitch, true)));
    let bare = PITCHES
        .rev()
        .map(|pitch| Style::Full(Pitch::new(pitch, false)));
    let brief = BRIEFS.map(Style::Brief);
    let mut board = None;
    for style in with_gauges.chain(bare).chain(brief) {
        let packed = pack(groups, width, style);
        if packed.height <= height {
            return packed;
        }
        board = Some(packed);
    }
    board.unwrap_or_else(|| pack(groups, width, Style::Brief(BRIEFS[1])))
}

/// Places each group's tiles in a grid under its title, and groups side by side while they fit the
/// width.
fn pack<'a>(groups: &[NodeGroup<'a>], width: f64, style: Style) -> Board<'a> {
    let mut board = Board {
        tiles: Vec::new(),
        labels: Vec::new(),
        height: 0.0,
        style,
    };
    let (mut x, mut y, mut shelf) = (MARGIN, MARGIN, 0.0_f64);
    for (index, group) in groups.iter().enumerate() {
        let (tile_w, tile_h) = group
            .nodes
            .iter()
            .map(|node| match &style {
                Style::Full(pitch) => pitch.tile(node),
                Style::Brief(brief) => brief.tile(node),
            })
            .fold((0.0_f64, 0.0_f64), |(w, h), (nw, nh)| {
                (w.max(nw), h.max(nh))
            });
        let count = group.nodes.len().max(1);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a small positive count"
        )]
        let fit = (((width - 2.0 * MARGIN + GAP) / (tile_w + GAP))
            .floor()
            .max(1.0)) as usize;
        let cols = count.min(fit);
        let rows = count.div_ceil(cols);
        #[expect(clippy::cast_precision_loss, reason = "a small count")]
        let (cols_f, rows_f) = (cols as f64, rows as f64);
        let block_w = (cols_f * tile_w + (cols_f - 1.0) * GAP).max(title_width(group));
        let block_h = LABEL + rows_f * tile_h + (rows_f - 1.0) * GAP;
        if x > MARGIN && x + block_w > width - MARGIN {
            x = MARGIN;
            y += shelf + SHELF_GAP;
            shelf = 0.0;
        }
        board.labels.push(Label {
            group: index,
            x,
            y,
            w: block_w,
        });
        for (i, node) in group.nodes.iter().enumerate() {
            #[expect(clippy::cast_precision_loss, reason = "a small count")]
            let (col, row) = ((i % cols) as f64, (i / cols) as f64);
            let frame = Rect {
                x: x + col * (tile_w + GAP),
                y: y + LABEL + row * (tile_h + GAP),
                w: tile_w,
                h: tile_h,
            };
            board.tiles.push(match &style {
                Style::Full(pitch) => place(node, frame, pitch),
                Style::Brief(brief) => place_brief(node, frame, brief),
            });
        }
        x += block_w + GROUP_GAP;
        shelf = shelf.max(block_h);
    }
    board.height = y + shelf + MARGIN;
    board
}

/// The widest a group's title and note can be, with its held counts as wide as its totals.
fn title_width(group: &NodeGroup) -> f64 {
    let cores: u32 = group.nodes.iter().map(|node| node.cores).sum();
    let gpus: u32 = group.nodes.iter().map(|node| node.gpus).sum();
    let note = note(
        group.nodes.len(),
        (u64::from(cores), u64::from(cores)),
        (u64::from(gpus), u64::from(gpus)),
    );
    #[expect(clippy::cast_precision_loss, reason = "a short text")]
    let width = group.name.chars().count() as f64 * TITLE_CHAR
        + 10.0
        + note.chars().count() as f64 * NOTE_CHAR;
    width
}

/// A group's note: its node count, and cores and GPUs as held and total.
fn note(count: usize, (cores_held, cores): (u64, u64), (gpus_held, gpus): (u64, u64)) -> String {
    let mut note = format!(
        "{count} node{} · {}/{} cores",
        if count == 1 { "" } else { "s" },
        thousands(cores_held),
        thousands(cores)
    );
    if gpus > 0 {
        let _ = write!(note, " · {gpus_held}/{gpus} gpus");
    }
    note
}

impl Brief {
    /// The GPU dots' side and the tile's inset.
    fn sizes(&self) -> (f64, f64) {
        if self.one_line {
            (5.0, 5.0)
        } else {
            (7.0, PAD)
        }
    }

    /// The tile's size: wider than its least width when one line must fit the name, a bar of at least
    /// 30 pixels and the GPU dots.
    fn tile(&self, node: &Node) -> (f64, f64) {
        if !self.one_line {
            return (self.w, self.h);
        }
        let (side, inset) = self.sizes();
        #[expect(clippy::cast_precision_loss, reason = "a short name")]
        let name_w = node.name.0.chars().count() as f64 * BRIEF_CHAR;
        let gpus_w = f64::from(node.gpus) * (side + 2.0) + 3.0;
        (
            self.w.max(2.0 * inset + name_w + 5.0 + 30.0 + gpus_w),
            self.h,
        )
    }
}

/// A node's name, bar and GPUs inside a brief tile `frame`. GPUs sit at the right, and on one line
/// the bar runs between the name and them.
fn place_brief<'a>(node: &'a Node, frame: Rect, brief: &Brief) -> Tile<'a> {
    let (side, inset) = brief.sizes();
    let gpu_x = |i: u32| frame.x + frame.w - inset - f64::from(node.gpus - i) * (side + 2.0) + 2.0;
    let gpus_left = gpu_x(0) - if node.gpus > 0 { 3.0 } else { 0.0 };
    let gpus = (0..node.gpus)
        .map(|i| Rect {
            x: gpu_x(i),
            y: frame.y + if brief.one_line { 6.5 } else { 5.0 },
            w: side,
            h: side,
        })
        .collect();
    #[expect(clippy::cast_precision_loss, reason = "a short name")]
    let name_w = node.name.0.chars().count() as f64 * BRIEF_CHAR;
    let bar = if brief.one_line {
        let x = frame.x + inset + name_w + 5.0;
        Rect {
            x,
            y: frame.y + 5.0,
            w: (gpus_left - x).max(4.0),
            h: 8.0,
        }
    } else {
        Rect {
            x: frame.x + inset,
            y: frame.y + 20.0,
            w: frame.w - 2.0 * inset,
            h: 8.0,
        }
    };
    Tile {
        node,
        frame,
        name: (frame.x + inset, frame.y + 13.0),
        cores: Vec::new(),
        bar: Some(bar),
        gpus,
    }
}

/// A node's cores and GPUs inside `frame`, centered.
fn place<'a>(node: &'a Node, frame: Rect, pitch: &Pitch) -> Tile<'a> {
    let shape = Shape::of(node);
    let (body_w, _) = pitch.body(&shape);
    let (grid_w, _) = pitch.grid(&shape);
    let (chips_w, chips_h) = pitch.chips(&shape);
    let left = frame.x + (frame.w - body_w) / 2.0;
    let top = frame.y + HEAD;
    let gpus = (0..shape.gpus)
        .map(|i| Rect {
            x: left
                + (body_w - chips_w) / 2.0
                + f64::from(i % shape.gpus_per_row) * (pitch.chip + pitch.chip_gap),
            y: top + f64::from(i / shape.gpus_per_row) * (pitch.chip + pitch.chip_gap),
            w: pitch.chip,
            h: pitch.chip,
        })
        .collect();
    let grid_top = if shape.gpus > 0 {
        top + chips_h + pitch.step
    } else {
        top
    };
    let grid_left = left + (body_w - grid_w) / 2.0;
    let socket_h = f64::from(shape.rows) * pitch.step - pitch.gap + pitch.socket_gap;
    let cores = (0..node.cores)
        .map(|core| {
            let socket = f64::from(core / shape.per_socket);
            let k = core % shape.per_socket;
            Rect {
                x: grid_left + f64::from(k % shape.cols) * pitch.step,
                y: grid_top + socket * socket_h + f64::from(k / shape.cols) * pitch.step,
                w: pitch.step - pitch.gap,
                h: pitch.step - pitch.gap,
            }
        })
        .collect();
    Tile {
        node,
        frame,
        name: (frame.x + PAD, frame.y + 14.0),
        cores,
        bar: None,
        gpus,
    }
}

/// The Tiles panel. Hovering a core shows the job that holds it, and hovering elsewhere on a tile shows
/// the node. Tiles fit the panel's visible height. When even the smallest tiles overflow it, the board
/// grows to its own height and the panel scrolls.
#[component]
pub fn Tiles(group: Grouping) -> impl IntoView {
    let state = expect_context::<State>();
    let tip = RwSignal::new(None::<Tip>);
    let panel = NodeRef::<leptos::html::Section>::new();
    let overflow = RwSignal::new(None::<f64>);
    let visible = move || {
        panel.get_untracked().map(|panel| {
            (
                f64::from(panel.client_width()),
                f64::from(panel.client_height()),
            )
        })
    };
    let point = move |event: web_sys::MouseEvent| {
        let (Some(element), Some((width, height))) = (panel.get_untracked(), visible()) else {
            return;
        };
        let frame = element.get_bounding_client_rect();
        let x = f64::from(event.client_x()) - frame.left();
        let y = f64::from(event.client_y()) - frame.top() + f64::from(element.scroll_top());
        let found = state.cluster.with_untracked(|cluster| {
            state.occupancy.with_untracked(|occupancy| {
                let groups = node_groups(cluster, group);
                let board = board(&groups, width, height);
                let (node, spot) = spot_at(&board, x, y)?;
                let lit = match owner_at(occupancy, node, spot) {
                    Some(Owner::Attempt(key)) => Highlight::Job(key),
                    _ => Highlight::Node(node.name.clone()),
                };
                Some((Tip::of(cluster, occupancy, node, spot), lit))
            })
        });
        let (found, lit) = found.unzip();
        tip.set(found.map(|tip| tip.at(x, y, width, height)));
        light_from_tiles(state, lit);
    };
    let draw = move |context: &CanvasRenderingContext2d, width: f64, height: f64| {
        let Some(theme) = state.theme.get() else {
            return;
        };
        let height = visible().map_or(height, |(_, height)| height);
        let highlight = state.highlight.get();
        let Some(coloring) = state.coloring.get() else {
            return;
        };
        let playhead = state.replay.get();
        state.cluster.with(|cluster| {
            state.occupancy.with(|occupancy| {
                let groups = node_groups(cluster, group);
                let board = board(&groups, width, height);
                let needed = (board.height > height).then(|| board.height.ceil());
                overflow.maybe_update(|overflow| std::mem::replace(overflow, needed) != needed);
                state.engine.with_value(|engine| {
                    let ink = Ink {
                        context,
                        theme: &theme,
                        cluster,
                        occupancy,
                        highlight: highlight.as_ref(),
                        coloring: &coloring,
                        effects: playhead.map(|playhead| (&engine.effects, playhead.at)),
                    };
                    ink.board(&groups, &board);
                });
            });
        });
    };
    view! {
        <section
            class="panel tiles"
            node_ref=panel
            on:mousemove=move |event| {
                if !state.tv {
                    point(event);
                }
            }
            on:mouseleave=move |_| {
                tip.set(None);
                light_from_tiles(state, None);
            }
        >
            <Legend/>
            <div
                class="board"
                style:min-height=move || overflow.get().map(|height| format!("{height}px"))
            >
                <Surface draw=draw/>
            </div>
            {move || tip.get().map(Tip::view)}
        </section>
    }
}

/// Under a palette by time left, each step's color with the time left at which it ends, then the rest
/// color for jobs with more. Nothing under any other palette.
#[component]
fn Legend() -> impl IntoView {
    let state = expect_context::<State>();
    move || {
        let theme = state.theme.get()?;
        let coloring = state.coloring.get()?;
        let swatch = |color: Rgb, label: String| {
            view! {
                <span>
                    <span class="swatch" style:background=color.css()></span>
                    {label}
                </span>
            }
        };
        let mut steps: Vec<_> = coloring
            .steps()
            .map(|(end_s, color)| {
                (
                    end_s,
                    swatch(painted(&theme, &coloring, Some(color)), horizon(end_s)),
                )
            })
            .collect();
        let (last, _) = steps.last()?;
        let rest = swatch(
            painted(&theme, &coloring, None),
            format!("{}+", horizon(*last)),
        );
        let steps: Vec<_> = steps.drain(..).map(|(_, step)| step).collect();
        Some(view! { <div class="legend">"time left" {steps} {rest}</div> })
    }
}

/// What drawing needs at hand.
struct Ink<'a> {
    context: &'a CanvasRenderingContext2d,
    theme: &'a Theme,
    cluster: &'a Cluster,
    occupancy: &'a Occupancies,
    highlight: Option<&'a Highlight>,
    coloring: &'a Coloring,
    /// While replaying, the cells that just changed hands and the playhead.
    effects: Option<(&'a Effects, f64)>,
}

impl Ink<'_> {
    fn rgb(color: &glurmo_model::theme::Color) -> Rgb {
        Rgb::of(color)
    }

    fn font(&self, weight: u32, size: f64, mono: bool) -> String {
        let family = if mono {
            &self.theme.fonts.mono
        } else {
            &self.theme.fonts.sans
        };
        let fallback = if mono { "monospace" } else { "sans-serif" };
        format!("{weight} {size}px \"{family}\", {fallback}")
    }

    fn text(&self, text: &str, x: f64, y: f64, font: &str, color: Rgb) {
        self.context.set_font(font);
        self.context.set_fill_style_str(&color.css());
        let _ = self.context.fill_text(text, x, y);
    }

    fn fill(&self, rect: Rect, radius: f64, color: Rgb) {
        self.context.set_fill_style_str(&color.css());
        self.context.begin_path();
        let _ = self
            .context
            .round_rect_with_f64(rect.x, rect.y, rect.w, rect.h, radius);
        self.context.fill();
    }

    fn board(&self, groups: &[NodeGroup], board: &Board) {
        for label in &board.labels {
            self.label(&groups[label.group], label);
        }
        for tile in &board.tiles {
            let faded = self.highlight.is_some_and(|light| light.fades(tile.node));
            self.context
                .set_global_alpha(if faded { FADED_TILE } else { 1.0 });
            match board.style {
                Style::Full(pitch) => self.tile(tile, pitch.gauges),
                Style::Brief(brief) => self.brief(tile, &brief),
            }
        }
        self.context.set_global_alpha(1.0);
    }

    fn label(&self, group: &NodeGroup, label: &Label) {
        let ui = &self.theme.ui;
        let baseline = label.y + 15.0;
        self.text(
            &group.name.to_uppercase(),
            label.x,
            baseline,
            &self.font(700, 12.0, false),
            Self::rgb(&ui.text),
        );
        let title_w = self
            .context
            .measure_text(&group.name.to_uppercase())
            .map_or(0.0, |metrics| metrics.width());
        let cores = group.cores(self.occupancy);
        let gpus = group.gpus(self.occupancy);
        let note = note(
            group.nodes.len(),
            (cores.held, cores.total),
            (gpus.held, gpus.total),
        );
        self.text(
            &note,
            label.x + title_w + 10.0,
            baseline,
            &self.font(500, 11.0, true),
            Self::rgb(&ui.muted),
        );
    }

    /// The color of a free cell on `node`: dimmed down or drain on a node that is out.
    fn empty(&self, node: &Node) -> Rgb {
        let free = Self::rgb(&self.theme.state.free);
        out(self.theme, node).map_or(free, |color| color.fade(free, OUT))
    }

    /// The color of a cell held by `owner`, or `empty` when no one holds it.
    fn cell(&self, owner: Option<Owner>, empty: Rgb) -> Rgb {
        let free = Self::rgb(&self.theme.state.free);
        let (color, login, key) = match owner {
            None => return empty,
            Some(Owner::Hidden) => (Self::rgb(&self.theme.ui.muted), None, None),
            Some(Owner::Attempt(key)) => match self.cluster.running.get(&key) {
                Some(running) => {
                    let login = &running.attempt.login;
                    let now = self.cluster.at.unwrap_or_default();
                    let color = self.coloring.core(&running.attempt, now);
                    (
                        painted(self.theme, self.coloring, color),
                        Some(login),
                        Some(key),
                    )
                }
                None => (Self::rgb(&self.theme.ui.muted), None, Some(key)),
            },
        };
        let lit = match self.highlight {
            Some(Highlight::User(lit)) => login == Some(lit),
            Some(Highlight::Group(lit)) => login
                .and_then(|login| self.cluster.users.get(login))
                .is_some_and(|user| user.groups.contains(lit)),
            Some(Highlight::Job(lit)) => key == Some(*lit),
            Some(Highlight::Jobs(lit)) => key.is_some_and(|key| lit.contains(&key)),
            _ => true,
        };
        if lit { color } else { color.fade(free, FADED) }
    }

    fn tile(&self, tile: &Tile, gauges: bool) {
        let ui = &self.theme.ui;
        let node = tile.node;
        let frame = tile.frame;
        self.fill(frame, 6.0, Self::rgb(&ui.surface));
        self.context
            .set_stroke_style_str(&Self::rgb(&ui.overlay).css());
        self.context.begin_path();
        let _ = self.context.round_rect_with_f64(
            frame.x + 0.5,
            frame.y + 0.5,
            frame.w - 1.0,
            frame.h - 1.0,
            6.0,
        );
        self.context.stroke();
        let (name_x, head) = tile.name;
        self.text(
            &node.name.0,
            name_x,
            head,
            &self.font(600, 12.0, true),
            Self::rgb(&ui.text),
        );
        if let (Some(word), Some(color)) = (node.trouble(), out(self.theme, node)) {
            self.context.set_text_align("right");
            self.text(
                &word.to_uppercase(),
                frame.x + frame.w - PAD,
                head,
                &self.font(700, 9.5, false),
                color,
            );
            self.context.set_text_align("left");
        }
        let empty = self.empty(node);
        let (cores, gpus) = self.cells(node);
        let radius = tile
            .cores
            .first()
            .map_or(1.0, |rect| if rect.w > 6.0 { 2.0 } else { 1.0 });
        let effects = self
            .effects
            .and_then(|(effects, now)| Some((effects.get(&node.name)?, now)));
        let effect = |cell| {
            let (cells, now) = effects?;
            let effect = cells.get(&cell)?;
            Some((effect, effect.strength(now)?))
        };
        for (index, (rect, owner)) in tile.cores.iter().zip(cores).enumerate() {
            let (rect, color) = self.lit(*rect, *owner, empty, effect(Cell::Core(index)));
            self.fill(rect, radius, color);
        }
        let stacks = memory_stacks(node);
        for (index, (rect, owner)) in tile.gpus.iter().zip(gpus).enumerate() {
            let (_, color) = self.lit(*rect, *owner, empty, effect(Cell::Gpu(index)));
            self.gpu(*rect, color, stacks);
        }
        if gauges {
            self.gauges(node, frame);
        }
    }

    /// A cell's rectangle and color, with a replay's effect on it at `strength`: a start flashes it
    /// toward the text color and swells it, and an end leaves the old holder's color fading on a free cell.
    fn lit(
        &self,
        rect: Rect,
        owner: Option<Owner>,
        empty: Rgb,
        effect: Option<(&Effect, f64)>,
    ) -> (Rect, Rgb) {
        let color = self.cell(owner, empty);
        match effect {
            Some((effect, strength)) if effect.started && owner.is_some() => {
                let grow = 1.5 * strength;
                let swollen = Rect {
                    x: rect.x - grow,
                    y: rect.y - grow,
                    w: rect.w + 2.0 * grow,
                    h: rect.h + 2.0 * grow,
                };
                let text = Self::rgb(&self.theme.ui.text);
                (swollen, color.fade(text, 1.0 - 0.85 * strength))
            }
            Some((effect, strength)) if !effect.started && owner.is_none() => {
                let held = painted(self.theme, self.coloring, self.coloring.user(&effect.login));
                (rect, held.fade(empty, 0.6 * strength))
            }
            _ => (rect, color),
        }
    }

    /// A GPU package outlined in `color`: its die filled with the color in the middle, and `stacks`
    /// memory stacks down each side in a darker shade, left out when too small to see.
    fn gpu(&self, rect: Rect, color: Rgb, stacks: u32) {
        let ui = &self.theme.ui;
        let surface = Self::rgb(&ui.surface);
        self.fill(rect, 3.0, Self::rgb(&ui.base).fade(surface, 0.5));
        self.context.set_stroke_style_str(&color.css());
        self.context.begin_path();
        let _ = self.context.round_rect_with_f64(
            rect.x + 0.5,
            rect.y + 0.5,
            rect.w - 1.0,
            rect.h - 1.0,
            3.0,
        );
        self.context.stroke();
        let (die_w, die_h) = ((rect.w * 0.44).round(), (rect.h * 0.5).round());
        let die = Rect {
            x: rect.x + (rect.w - die_w) / 2.0,
            y: rect.y + (rect.h - die_h) / 2.0,
            w: die_w,
            h: die_h,
        };
        let count = f64::from(stacks);
        let stack_h = (die.h - (count - 1.0) * 2.0) / count;
        let stack_w = (rect.w - die.w) / 2.0 - 5.0;
        if stack_h >= 2.0 && stack_w >= 2.0 {
            for i in 0..stacks {
                for x in [die.x - stack_w - 2.0, die.x + die.w + 2.0] {
                    let stack = Rect {
                        x,
                        y: die.y + f64::from(i) * (stack_h + 2.0),
                        w: stack_w,
                        h: stack_h,
                    };
                    self.fill(stack, 0.5, color.fade(surface, 0.65));
                }
            }
        }
        self.fill(die, 1.0, color);
    }

    /// Load against CPUs, and memory allocated with what the OS has in use as a tick. The tracks start
    /// after the wider label, and are left out when the tile has no room for them.
    fn gauges(&self, node: &Node, frame: Rect) {
        let ui = &self.theme.ui;
        let font = self.font(500, 10.0, true);
        let first = frame.y + frame.h - FOOT + PAD + GAUGE_RISE;
        let second = first + GAUGE_ROW;
        let load_label = format!("load {}", node.load.map_or_else(|| "-".to_owned(), load));
        let mem_label = format!(
            "mem {}/{}",
            gigabytes(node.mem_alloc_mb),
            gigabytes(node.mem_mb)
        );
        self.text(
            &load_label,
            frame.x + PAD,
            first,
            &font,
            Self::rgb(&ui.subtle),
        );
        self.text(
            &mem_label,
            frame.x + PAD,
            second,
            &font,
            Self::rgb(&ui.subtle),
        );
        let label_w = [&load_label, &mem_label]
            .iter()
            .filter_map(|label| self.context.measure_text(label).ok())
            .map(|metrics| metrics.width())
            .fold(0.0, f64::max);
        let track_x = frame.x + PAD + label_w + 8.0;
        let track_w = frame.x + frame.w - PAD - track_x;
        if track_w < MIN_TRACK {
            return;
        }
        let cpus = f64::from(node.cpus.max(1));
        self.track(
            track_x,
            first - 3.5,
            track_w,
            None,
            node.load.map(|load| load / cpus),
        );
        let mem = node.mem_mb.max(1);
        #[expect(clippy::cast_precision_loss, reason = "megabytes fit in an f64")]
        let fraction = |mb: u64| mb as f64 / mem as f64;
        let used = node
            .mem_free_mb
            .map(|free| fraction(node.mem_mb.saturating_sub(free)));
        self.track(
            track_x,
            second - 3.5,
            track_w,
            Some(fraction(node.mem_alloc_mb)),
            used,
        );
    }

    /// A thin track with an optional band from its start and an optional tick, both as fractions.
    fn track(&self, x: f64, y: f64, w: f64, band: Option<f64>, tick: Option<f64>) {
        let ui = &self.theme.ui;
        self.fill(Rect { x, y, w, h: 3.0 }, 1.5, Self::rgb(&ui.overlay));
        if let Some(band) = band {
            let band_w = w * band.clamp(0.0, 1.0);
            self.fill(
                Rect {
                    x,
                    y,
                    w: band_w,
                    h: 3.0,
                },
                1.5,
                Self::rgb(&ui.muted),
            );
        }
        if let Some(tick) = tick {
            let at = x + w * tick.clamp(0.0, 1.0);
            self.fill(
                Rect {
                    x: at - 1.0,
                    y: y - 3.0,
                    w: 2.0,
                    h: 9.0,
                },
                1.0,
                Self::rgb(&ui.text),
            );
        }
    }

    /// A node's name and a bar of its cores by user, then its GPUs.
    fn brief(&self, tile: &Tile, brief: &Brief) {
        let ui = &self.theme.ui;
        let node = tile.node;
        let frame = tile.frame;
        self.fill(frame, 4.0, Self::rgb(&ui.surface));
        let name_color = out(self.theme, node).unwrap_or_else(|| Self::rgb(&ui.text));
        let size = if brief.one_line { 10.0 } else { 10.5 };
        self.text(
            &node.name.0,
            tile.name.0,
            tile.name.1,
            &self.font(600, size, true),
            name_color,
        );
        let empty = self.empty(node);
        let (cores, gpus) = self.cells(node);
        let Some(bar) = tile.bar else {
            return;
        };
        self.fill(bar, 2.0, empty);
        let mut runs: Vec<(Rgb, usize)> = Vec::new();
        for owner in cores.iter().flatten() {
            let color = self.cell(Some(*owner), empty);
            match runs.iter_mut().find(|(run, _)| *run == color) {
                Some((_, count)) => *count += 1,
                None => runs.push((color, 1)),
            }
        }
        runs.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        #[expect(clippy::cast_precision_loss, reason = "a small count")]
        let per_core = bar.w / cores.len().max(1) as f64;
        let mut x = bar.x;
        for (color, count) in runs {
            #[expect(clippy::cast_precision_loss, reason = "a small count")]
            let w = per_core * count as f64;
            self.fill(Rect { x, w, ..bar }, 0.0, color);
            x += w;
        }
        for (rect, owner) in tile.gpus.iter().zip(gpus) {
            self.fill(*rect, 1.5, self.cell(*owner, empty));
        }
    }

    fn cells(&self, node: &Node) -> (&[Option<Owner>], &[Option<Owner>]) {
        self.occupancy
            .get(&node.name)
            .map_or((&[][..], &[][..]), |cells| {
                (cells.cores.as_slice(), cells.gpus.as_slice())
            })
    }
}

/// HBM stacks on each side of a GPU's die: a V100 has four in all, and newer parts such as the H200 six.
fn memory_stacks(node: &Node) -> u32 {
    match node.gpu_model.as_deref() {
        Some("V100") => 2,
        _ => 3,
    }
}

/// The color of a node that is out: down or draining. `None` for a node that takes jobs.
fn out(theme: &Theme, node: &Node) -> Option<Rgb> {
    match node.state {
        _ if node.flags.contains(&NodeFlag::NotResponding) => Some(Rgb::of(&theme.state.down)),
        NodeState::Down | NodeState::Unknown => Some(Rgb::of(&theme.state.down)),
        NodeState::Draining | NodeState::Drained => Some(Rgb::of(&theme.state.drain)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glurmo_model::{NodeName, NodeState};

    fn node(name: &str, cores: u32, gpus: u32) -> Node {
        Node {
            name: NodeName(name.into()),
            partitions: Vec::new(),
            state: NodeState::Idle,
            flags: Vec::new(),
            state_raw: "IDLE".into(),
            reason: None,
            cpus: cores * 2,
            cpus_alloc: 0,
            sockets: 2,
            cores,
            load: None,
            mem_mb: 0,
            mem_alloc_mb: 0,
            mem_free_mb: None,
            gpus,
            gpus_used: 0,
            gpu_model: None,
            features: Vec::new(),
        }
    }

    #[test]
    fn a_point_finds_its_core_its_gpu_or_its_tile() {
        let nodes = [node("z1", 4, 0), node("g1", 4, 2)];
        let groups = [group("cpu", &nodes[..1]), group("gpu", &nodes[1..])];
        let board = board(&groups, 800.0, 600.0);
        let middle = |rect: &Rect| (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
        let cpu = &board.tiles[0];
        let (x, y) = middle(&cpu.cores[3]);
        assert_eq!(spot_at(&board, x, y), Some((&nodes[0], Spot::Core(3))));
        let (x, y) = middle(&board.tiles[1].gpus[1]);
        assert_eq!(spot_at(&board, x, y), Some((&nodes[1], Spot::Gpu(1))));
        let gap_x = cpu.cores[0].x + cpu.cores[0].w + 0.5;
        assert_eq!(
            spot_at(&board, gap_x, cpu.cores[0].y).map(|(_, spot)| spot),
            Some(Spot::Core(0)),
            "a point in the gap after a core"
        );
        let name = (cpu.frame.x + 2.0, cpu.frame.y + 2.0);
        assert_eq!(
            spot_at(&board, name.0, name.1),
            Some((&nodes[0], Spot::Tile))
        );
        assert_eq!(spot_at(&board, 799.0, 599.0), None);
    }

    #[test]
    fn partition_and_node_highlights_fade_the_tiles_they_leave_out() {
        let mut inside = node("z1", 4, 0);
        inside.partitions = vec![glurmo_model::PartitionName("long".into())];
        let outside = node("z2", 4, 0);
        let partition = Highlight::Partition(glurmo_model::PartitionName("long".into()));
        assert!(!partition.fades(&inside));
        assert!(partition.fades(&outside));
        let one = Highlight::Node(NodeName("z2".into()));
        assert!(one.fades(&inside));
        assert!(!one.fades(&outside));
        assert!(!Highlight::User(glurmo_model::Login("otter".into())).fades(&outside));
    }

    fn group<'a>(name: &str, nodes: &'a [Node]) -> NodeGroup<'a> {
        NodeGroup {
            name: name.into(),
            nodes: nodes.iter().collect(),
        }
    }

    #[test]
    fn a_roomy_panel_gets_the_largest_cores() {
        let nodes = [node("z1", 52, 0)];
        let board = board(&[group("cpu", &nodes)], 1_000.0, 1_000.0);
        assert!(matches!(board.style, Style::Full(_)));
        let core = board.tiles[0].cores[0];
        assert_eq!(core.w, 16.0, "pitch 18 less a gap of 2");
        assert_eq!(board.tiles[0].cores.len(), 52);
    }

    #[test]
    fn cores_shrink_until_the_board_fits_then_go_brief() {
        let nodes: Vec<Node> = (0..40).map(|i| node(&format!("z{i}"), 52, 0)).collect();
        let groups = [group("cpu", &nodes)];
        let roomy = board(&groups, 1_400.0, 2_000.0);
        let tight = board(&groups, 1_400.0, 600.0);
        assert!(tight.tiles[0].cores[0].w < roomy.tiles[0].cores[0].w);
        assert!(tight.height <= 600.0);
        let cramped = board(&groups, 1_400.0, 150.0);
        assert!(matches!(cramped.style, Style::Brief(_)));
        assert_eq!(cramped.tiles[0].cores, []);
    }

    #[test]
    fn sockets_stack_and_gpus_sit_above_the_cores() {
        let nodes = [node("g1", 16, 4)];
        let board = board(&[group("gpu", &nodes)], 600.0, 600.0);
        let tile = &board.tiles[0];
        let first_socket = tile.cores[0].y;
        let second_socket = tile.cores[8].y;
        assert!(second_socket > first_socket, "core 8 starts socket 1");
        assert_eq!(tile.gpus.len(), 4);
        assert!(tile.gpus.iter().all(|gpu| gpu.y < first_socket));
        for rect in tile.cores.iter().chain(&tile.gpus) {
            assert!(rect.x >= tile.frame.x && rect.x + rect.w <= tile.frame.x + tile.frame.w);
        }
    }

    #[test]
    fn small_groups_share_a_row_and_big_ones_wrap() {
        let cpu: Vec<Node> = (0..3).map(|i| node(&format!("c{i}"), 16, 0)).collect();
        let gpu = [node("g1", 16, 4)];
        let board = board(&[group("cpu", &cpu), group("gpu", &gpu)], 2_000.0, 2_000.0);
        assert_eq!(board.labels[0].y, board.labels[1].y, "side by side");
        assert!(board.labels[1].x > board.labels[0].x + board.labels[0].w);
        let narrow = super::board(&[group("cpu", &cpu), group("gpu", &gpu)], 300.0, 2_000.0);
        assert!(narrow.labels[1].y > narrow.labels[0].y, "wrapped");
    }

    #[test]
    fn a_one_node_group_leaves_room_for_its_title() {
        let skylake = [node("z1054", 36, 0)];
        let gpu = [node("g1", 16, 4)];
        let board = board(
            &[group("skylake", &skylake), group("gpu", &gpu)],
            2_000.0,
            2_000.0,
        );
        let first = &board.labels[0];
        assert!(first.w >= title_width(&group("skylake", &skylake)));
        assert!(
            board.labels[1].x >= first.x + first.w,
            "titles do not overlap"
        );
    }

    /// zlurm's node shapes in 2026: 24, 14 and 1 CPU nodes, then 9 V100 nodes and 3 big GPU nodes.
    fn zlurm() -> [Vec<Node>; 5] {
        let many = |count: usize, cores, gpus| -> Vec<Node> {
            (0..count)
                .map(|i| node(&format!("n{i}"), cores, gpus))
                .collect()
        };
        [
            many(24, 52, 0),
            many(14, 44, 0),
            many(1, 36, 0),
            many(9, 16, 4),
            vec![
                node("js029", 128, 8),
                node("js031", 128, 8),
                node("z028", 128, 6),
            ],
        ]
    }

    #[test]
    fn zlurm_fits_full_tiles_in_a_tv_die_panel() {
        let nodes = zlurm();
        let groups: Vec<NodeGroup> = nodes.iter().map(|nodes| group("g", nodes)).collect();
        let board = board(&groups, 1_330.0, 690.0);
        assert!(matches!(board.style, Style::Full(_)));
        let roomy = super::board(&groups, 1_330.0, 2_000.0);
        assert!(
            matches!(roomy.style, Style::Full(Pitch { gauges: true, .. })),
            "gauges come back when there is room"
        );
    }

    #[test]
    fn zlurm_fits_a_small_tv_in_one_line_tiles() {
        let nodes = zlurm();
        let groups: Vec<NodeGroup> = nodes.iter().map(|nodes| group("g", nodes)).collect();
        let board = board(&groups, 940.0, 400.0);
        assert!(board.height <= 400.0, "{}", board.height);
    }

    #[test]
    fn one_line_tiles_widen_to_keep_a_bar_beside_many_gpus() {
        let nodes = [node("js029", 128, 8)];
        let tile = place_brief(
            &nodes[0],
            Rect {
                x: 0.0,
                y: 0.0,
                w: BRIEFS[1].tile(&nodes[0]).0,
                h: 18.0,
            },
            &BRIEFS[1],
        );
        assert!(tile.bar.unwrap().w >= 30.0);
        assert!(
            tile.gpus
                .iter()
                .all(|gpu| gpu.x > tile.bar.unwrap().x + 30.0)
        );
    }
}
