//! What every panel reads: the cluster as the stream builds it, the picked theme and layout, and what
//! the page's address asks for.

use std::collections::{BTreeMap, BTreeSet};

use leptos::prelude::*;

use crate::replay::{Engine, Playhead};
use crate::tree::Path;
use glurmo_model::layout::{Layout, Pane};
use glurmo_model::palette::{Coloring, Palette};
use glurmo_model::summary::Ticker;
use glurmo_model::theme::Theme;
use glurmo_model::usage::Usage;
use glurmo_model::{
    AttemptKey, Cluster, GroupName, Login, Node, NodeName, Occupancies, PartitionName, UnixTime,
};

/// The theme drawn with when the address names none, or one that does not exist.
pub const DEFAULT_THEME: &str = "rose-pine";

/// The layout shown when the address names none, or one that does not exist.
pub const DEFAULT_LAYOUT: &str = "web";

/// The state of the stream from the server.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    /// Not connected yet.
    Connecting,
    /// Following the stream.
    Live,
    /// The stream broke at this many milliseconds since the epoch, and the browser is reconnecting.
    Lost(f64),
}

/// What the Tiles panel lights up while everything else fades.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Highlight {
    /// One user's cores and GPUs.
    User(Login),
    /// The cores and GPUs of one group's members.
    Group(GroupName),
    /// The nodes in one partition.
    Partition(PartitionName),
    /// One node.
    Node(NodeName),
    /// The job under the pointer on the Tiles panel. The job list lights its row and scrolls to it.
    Job(AttemptKey),
    /// The jobs in a row of the job list.
    Jobs(BTreeSet<AttemptKey>),
}

impl Highlight {
    /// Whether `node`'s whole tile fades. A user or job highlight fades cores, not tiles.
    pub fn fades(&self, node: &Node) -> bool {
        match self {
            Self::User(_) | Self::Group(_) | Self::Job(_) | Self::Jobs(_) => false,
            Self::Partition(partition) => !node.partitions.contains(partition),
            Self::Node(name) => node.name != *name,
        }
    }
}

/// The page's state. Every field is a signal, so the whole is `Copy` and panels take it from context.
#[derive(Clone, Copy)]
pub struct State {
    /// The cluster.
    pub cluster: RwSignal<Cluster>,
    /// Who holds each core and GPU, worked out once per change.
    pub occupancy: Memo<Occupancies>,
    /// The cluster's name, from the snapshot.
    pub cluster_name: RwSignal<String>,
    /// The stream.
    pub link: RwSignal<Link>,
    /// Recent starts and ends, latest first.
    pub ticker: RwSignal<Ticker>,
    /// Usage history, or why it could not be read.
    pub usage: RwSignal<Result<Usage, String>>,
    /// Themes by file name.
    pub themes: RwSignal<BTreeMap<String, Theme>>,
    /// Layouts by file name.
    pub layouts: RwSignal<BTreeMap<String, Layout>>,
    /// Palettes by file name.
    pub palettes: RwSignal<BTreeMap<String, Palette>>,
    /// The theme asked for.
    pub theme_name: RwSignal<String>,
    /// The palette asked for. Empty asks for the theme's own user colors.
    pub palette_name: RwSignal<String>,
    /// The layout asked for.
    pub layout_name: RwSignal<String>,
    /// The theme being edited, which every panel draws with until the editor closes.
    pub draft_theme: RwSignal<Option<Theme>>,
    /// The layout being edited, shown until the editor closes. The editor is open while it is set.
    pub draft_layout: RwSignal<Option<Layout>>,
    /// The palette being edited, which colors every panel until the editor closes.
    pub draft_palette: RwSignal<Option<Palette>>,
    /// The palette that colors the page: the draft, else the one asked for, else the theme's own.
    pub palette: Memo<Option<Palette>>,
    /// The theme drawn with: the draft, else the one asked for once themes have loaded.
    pub theme: Memo<Option<Theme>>,
    /// The layout shown: the draft, else the one asked for once layouts have loaded.
    pub layout: Memo<Option<Layout>>,
    /// Who takes which color: the palette asked for, else the theme's own, worked out for the cluster
    /// as it stands. `None` until a theme has loaded.
    pub coloring: Memo<Option<Coloring>>,
    /// Whether the editor is open. A memo, so views that only care whether it is open are not rebuilt
    /// on every edit of the draft.
    pub editing: Memo<bool>,
    /// What stands out on the Tiles panel while the rest fades.
    pub highlight: RwSignal<Option<Highlight>>,
    /// Copies of the highlight for table rows.
    pub row_highlights: RowHighlights,
    /// Whoever the address names with `me`.
    pub me: RwSignal<Option<Login>>,
    /// Counts fonts finished loading, so canvases redraw in them.
    pub fonts: RwSignal<u32>,
    /// Device pixels per CSS pixel on screen, so canvases stay sharp under a pinch or browser zoom.
    pub zoom: RwSignal<f64>,
    /// The browser's clock, ticking each second.
    pub now: RwSignal<UnixTime>,
    /// What stretch of time the History panels show.
    pub timeline: RwSignal<Timeline>,
    /// Where a replay is, or `None` while the view is live.
    pub replay: RwSignal<Option<Playhead>>,
    /// What a replay keeps between frames, including the live view it set aside.
    pub engine: StoredValue<Engine>,
    /// The panel filling the stage in place of the layout, if any.
    pub filled: RwSignal<Option<Path>>,
    /// The panel under the pointer, which `f` fills.
    pub pointed: StoredValue<Option<Path>>,
    /// Whether this is a wall display: no cursor, and the highlight cycles through users.
    pub tv: bool,
}

/// Copies of the highlight for table rows, which a table rebuilds on every change. `reactive_graph` 0.2
/// keeps a dropped reader in a signal's subscriber list for as long as the signal lives, and walks the
/// list on every write. Rows that read the page's highlight would slow each hover a little more with
/// every rebuild. A copy dies with the rows that read it, and its list with it.
#[derive(Clone, Copy)]
pub struct RowHighlights(StoredValue<Vec<RwSignal<Option<Highlight>>>>);

impl RowHighlights {
    fn new() -> Self {
        Self(StoredValue::new(Vec::new()))
    }

    /// A copy that starts at `highlight` and lives as long as the current owner.
    fn copy(self, highlight: Option<Highlight>) -> ReadSignal<Option<Highlight>> {
        let copy = RwSignal::new(highlight);
        self.0.update_value(|copies| {
            copies.retain(|copy| !copy.is_disposed());
            copies.push(copy);
        });
        copy.read_only()
    }

    /// Sets every live copy to `highlight`, and forgets the dead ones.
    fn send(self, highlight: Option<&Highlight>) {
        self.0.update_value(|copies| {
            copies.retain(|copy| copy.try_set(highlight.cloned()).is_none());
        });
    }
}

/// What stretch of time the History panels show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    /// How long, in seconds, or `None` for the layout's span.
    pub span_s: Option<i64>,
    /// Where it ends, or `None` to follow now.
    pub end: Option<UnixTime>,
}

/// What the address asks for, as in `?layout=tv&theme=rose-pine-dawn&palette=mono&me=otter&tv&at=1794700000&speed=60`.
pub struct Address {
    /// The layout.
    pub layout: String,
    /// The theme.
    pub theme: String,
    /// The palette, or the theme's own user colors when none is named.
    pub palette: Option<String>,
    /// The viewer's login.
    pub me: Option<String>,
    /// Whether to run as a wall display. A layout named `tv` implies it.
    pub tv: bool,
    /// A moment to replay from, in seconds since the epoch, given as such or as a date the browser reads.
    pub at: Option<f64>,
    /// How fast to replay it, or paused.
    pub speed: Option<u32>,
}

impl Address {
    /// Reads the page's address.
    pub fn read() -> Self {
        let search = window().location().search().unwrap_or_default();
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok();
        let get = |name: &str| params.as_ref().and_then(|params| params.get(name));
        let layout = get("layout").unwrap_or_else(|| DEFAULT_LAYOUT.to_owned());
        let tv = params.as_ref().is_some_and(|params| params.has("tv")) || layout == "tv";
        let at = get("at").and_then(|at| {
            at.parse()
                .ok()
                .or_else(|| Some(js_sys::Date::parse(&at) / 1_000.0).filter(|at| at.is_finite()))
        });
        Self {
            theme: get("theme").unwrap_or_else(|| DEFAULT_THEME.to_owned()),
            palette: get("palette"),
            me: get("me"),
            layout,
            tv,
            at,
            speed: get("speed").and_then(|speed| speed.parse().ok()),
        }
    }
}

impl State {
    /// A state with nothing loaded yet, picking what `address` asks for.
    pub fn new(address: Address) -> Self {
        let cluster = RwSignal::new(Cluster::default());
        let themes = RwSignal::new(BTreeMap::<String, Theme>::new());
        let layouts = RwSignal::new(BTreeMap::new());
        let theme_name = RwSignal::new(address.theme);
        let layout_name = RwSignal::new(address.layout);
        let draft_theme = RwSignal::new(None);
        let draft_layout = RwSignal::new(None);
        let highlight = RwSignal::new(None);
        let row_highlights = RowHighlights::new();
        Effect::new(move |_| highlight.with(|lit| row_highlights.send(lit.as_ref())));
        let occupancy = Memo::new(move |_| cluster.with(Cluster::occupancy));
        let theme = Memo::new(move |_| {
            draft_theme.get().or_else(|| {
                themes.with(|themes| {
                    pick(themes, &theme_name.get(), DEFAULT_THEME).map(|(_, theme)| theme.clone())
                })
            })
        });
        let palettes = RwSignal::new(BTreeMap::<String, Palette>::new());
        let palette_name = RwSignal::new(address.palette.unwrap_or_default());
        let draft_palette = RwSignal::new(None::<Palette>);
        let palette = Memo::new(move |_| {
            draft_palette
                .get()
                .or_else(|| palettes.with(|palettes| palettes.get(&palette_name.get()).cloned()))
                .or_else(|| theme.with(|theme| theme.as_ref().map(Palette::of_theme)))
        });
        let coloring = Memo::new(move |_| {
            palette.with(|palette| {
                let palette = palette.as_ref()?;
                Some(cluster.with(|cluster| {
                    occupancy.with(|occupancy| palette.coloring(cluster, occupancy))
                }))
            })
        });
        Self {
            cluster,
            occupancy,
            cluster_name: RwSignal::new(String::new()),
            link: RwSignal::new(Link::Connecting),
            ticker: RwSignal::new(Ticker::default()),
            usage: RwSignal::new(Ok(Usage::default())),
            themes,
            layouts,
            palettes,
            theme_name,
            palette_name,
            layout_name,
            draft_theme,
            draft_layout,
            draft_palette,
            palette,
            theme,
            coloring,
            layout: Memo::new(move |_| {
                draft_layout.get().or_else(|| {
                    layouts.with(|layouts| {
                        pick(layouts, &layout_name.get(), DEFAULT_LAYOUT)
                            .map(|(_, layout)| layout.clone())
                    })
                })
            }),
            editing: Memo::new(move |_| draft_layout.with(Option::is_some)),
            highlight,
            row_highlights,
            me: RwSignal::new(address.me.map(Login)),
            fonts: RwSignal::new(0),
            zoom: RwSignal::new(crate::paint::zoom()),
            now: RwSignal::new(crate::paint::now()),
            timeline: RwSignal::new(Timeline::default()),
            replay: RwSignal::new(None),
            engine: StoredValue::new(Engine::default()),
            filled: RwSignal::new(None),
            pointed: StoredValue::new(None),
            tv: address.tv,
        }
    }

    /// A copy of the highlight for rows built under the current owner, which dies with them.
    pub fn row_highlight(&self) -> ReadSignal<Option<Highlight>> {
        self.row_highlights.copy(self.highlight.get_untracked())
    }

    /// How far back the History panels look, in seconds: the span picked on the timeline, else the
    /// longest the layout gives, else a day.
    pub fn history_span_s(&self) -> i64 {
        self.timeline
            .with(|timeline| timeline.span_s)
            .unwrap_or_else(|| {
                self.layout.with(|layout| {
                    layout
                        .as_ref()
                        .and_then(|layout| longest_history(&layout.root))
                        .unwrap_or(86_400)
                })
            })
    }
}

/// The entry named `name`, else `fallback`, else the first, with the name it goes by.
pub fn pick<'a, T>(
    entries: &'a BTreeMap<String, T>,
    name: &str,
    fallback: &str,
) -> Option<(&'a String, &'a T)> {
    entries
        .get_key_value(name)
        .or_else(|| entries.get_key_value(fallback))
        .or_else(|| entries.iter().next())
}

fn longest_history(pane: &Pane) -> Option<i64> {
    match pane {
        Pane::Split(_, children) => children
            .iter()
            .filter_map(|(_, child)| longest_history(child))
            .max(),
        Pane::History { span } => Some(span.seconds()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_names_the_entry_it_settles_on() {
        let entries = BTreeMap::from([("dawn".to_owned(), 1), ("moon".to_owned(), 2)]);
        let named =
            |name: &str, fallback| pick(&entries, name, fallback).map(|(name, _)| name.as_str());
        assert_eq!(named("moon", "dawn"), Some("moon"));
        assert_eq!(named("noon", "moon"), Some("moon"));
        assert_eq!(named("noon", "dusk"), Some("dawn"));
        assert_eq!(pick(&BTreeMap::<String, u8>::new(), "noon", "dusk"), None);
    }

    #[test]
    fn row_highlights_keep_only_the_copies_of_live_rows() {
        let root = Owner::new();
        root.set();
        let copies = RowHighlights::new();
        let mut rows = root.child();
        for _ in 0..200 {
            rows.cleanup();
            rows = root.child();
            rows.with(|| copies.copy(None));
        }
        let lit = rows.with(|| copies.copy(None));
        let node = Some(Highlight::Node(NodeName("z1".into())));
        copies.send(node.as_ref());
        assert_eq!(lit.get_untracked(), node);
        assert_eq!(copies.0.with_value(Vec::len), 2);
        rows.cleanup();
        copies.send(None);
        assert_eq!(copies.0.with_value(Vec::len), 0);
    }
}
