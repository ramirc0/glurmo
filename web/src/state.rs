//! What every panel reads: the cluster as the stream builds it, the picked theme and layout, and what
//! the page's address asks for.

use std::collections::BTreeMap;

use leptos::prelude::*;

use crate::replay::{Engine, Playhead};
use rtviz_model::layout::{Layout, Pane};
use rtviz_model::summary::Ticker;
use rtviz_model::theme::Theme;
use rtviz_model::usage::Usage;
use rtviz_model::{Cluster, Login, Node, NodeName, Occupancies, PartitionName, UnixTime};

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

/// What the Die lights up while everything else fades.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Highlight {
    /// One user's cores and GPUs.
    User(Login),
    /// The nodes in one partition.
    Partition(PartitionName),
    /// One node.
    Node(NodeName),
}

impl Highlight {
    /// Whether `node`'s whole tile fades. A user highlight fades cores, not tiles.
    pub fn fades(&self, node: &Node) -> bool {
        match self {
            Self::User(_) => false,
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
    /// The theme asked for.
    pub theme_name: RwSignal<String>,
    /// The layout asked for.
    pub layout_name: RwSignal<String>,
    /// The theme being edited, which every panel draws with until the editor closes.
    pub draft_theme: RwSignal<Option<Theme>>,
    /// The layout being edited, shown until the editor closes. The editor is open while it is set.
    pub draft_layout: RwSignal<Option<Layout>>,
    /// The theme drawn with: the draft, else the one asked for once themes have loaded.
    pub theme: Memo<Option<Theme>>,
    /// The layout shown: the draft, else the one asked for once layouts have loaded.
    pub layout: Memo<Option<Layout>>,
    /// What stands out on the Die while the rest fades.
    pub highlight: RwSignal<Option<Highlight>>,
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
    /// Whether this is a wall display: no cursor, and the highlight cycles through users.
    pub tv: bool,
}

/// What stretch of time the History panels show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    /// How long, in seconds, or `None` for the layout's span.
    pub span_s: Option<i64>,
    /// Where it ends, or `None` to follow now.
    pub end: Option<UnixTime>,
}

/// What the address asks for, as in `?layout=tv&theme=rose-pine-dawn&me=otter&tv&at=1794700000&speed=60`.
pub struct Address {
    /// The layout.
    pub layout: String,
    /// The theme.
    pub theme: String,
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
        let layout = get("layout").unwrap_or_else(|| "web".to_owned());
        let tv = params.as_ref().is_some_and(|params| params.has("tv")) || layout == "tv";
        let at = get("at").and_then(|at| {
            at.parse()
                .ok()
                .or_else(|| Some(js_sys::Date::parse(&at) / 1_000.0).filter(|at| at.is_finite()))
        });
        Self {
            theme: get("theme").unwrap_or_else(|| "rose-pine".to_owned()),
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
        let themes = RwSignal::new(BTreeMap::new());
        let layouts = RwSignal::new(BTreeMap::new());
        let theme_name = RwSignal::new(address.theme);
        let layout_name = RwSignal::new(address.layout);
        let draft_theme = RwSignal::new(None);
        let draft_layout = RwSignal::new(None);
        Self {
            cluster,
            occupancy: Memo::new(move |_| cluster.with(Cluster::occupancy)),
            cluster_name: RwSignal::new(String::new()),
            link: RwSignal::new(Link::Connecting),
            ticker: RwSignal::new(Ticker::default()),
            usage: RwSignal::new(Ok(Usage::default())),
            themes,
            layouts,
            theme_name,
            layout_name,
            draft_theme,
            draft_layout,
            theme: Memo::new(move |_| {
                draft_theme.get().or_else(|| {
                    themes.with(|themes| pick(themes, &theme_name.get(), "rose-pine").cloned())
                })
            }),
            layout: Memo::new(move |_| {
                draft_layout.get().or_else(|| {
                    layouts.with(|layouts| pick(layouts, &layout_name.get(), "web").cloned())
                })
            }),
            highlight: RwSignal::new(None),
            me: RwSignal::new(address.me.map(Login)),
            fonts: RwSignal::new(0),
            zoom: RwSignal::new(crate::paint::zoom()),
            now: RwSignal::new(crate::paint::now()),
            timeline: RwSignal::new(Timeline::default()),
            replay: RwSignal::new(None),
            engine: StoredValue::new(Engine::default()),
            tv: address.tv,
        }
    }

    /// Whether the editor is open.
    pub fn editing(&self) -> bool {
        self.draft_layout.with(Option::is_some)
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

/// The entry named `name`, else `fallback`, else the first.
fn pick<'a, T>(entries: &'a BTreeMap<String, T>, name: &str, fallback: &str) -> Option<&'a T> {
    entries
        .get(name)
        .or_else(|| entries.get(fallback))
        .or_else(|| entries.values().next())
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
