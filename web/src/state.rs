//! What every panel reads: the cluster as the stream builds it, the picked theme and layout, and what
//! the page's address asks for.

use std::collections::BTreeMap;

use leptos::prelude::*;
use rtviz_model::layout::{Layout, Pane};
use rtviz_model::summary::Ticker;
use rtviz_model::theme::Theme;
use rtviz_model::usage::Usage;
use rtviz_model::{Cluster, Login, Occupancies};

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
    /// The theme drawn with, once themes have loaded.
    pub theme: Memo<Option<Theme>>,
    /// The layout shown, once layouts have loaded.
    pub layout: Memo<Option<Layout>>,
    /// A user whose cores stand out while everyone else's fade.
    pub highlight: RwSignal<Option<Login>>,
    /// Whoever the address names with `me`.
    pub me: RwSignal<Option<Login>>,
    /// Whether this is a wall display: no cursor, and the highlight cycles through users.
    pub tv: bool,
}

/// What the address asks for, as in `?layout=tv&theme=rose-pine-dawn&me=otter&tv`.
pub struct Address {
    /// The layout.
    pub layout: String,
    /// The theme.
    pub theme: String,
    /// The viewer's login.
    pub me: Option<String>,
    /// Whether to run as a wall display. A layout named `tv` implies it.
    pub tv: bool,
}

impl Address {
    /// Reads the page's address.
    pub fn read() -> Self {
        let search = window().location().search().unwrap_or_default();
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok();
        let get = |name: &str| params.as_ref().and_then(|params| params.get(name));
        let layout = get("layout").unwrap_or_else(|| "web".to_owned());
        let tv = params.as_ref().is_some_and(|params| params.has("tv")) || layout == "tv";
        Self {
            theme: get("theme").unwrap_or_else(|| "rose-pine".to_owned()),
            me: get("me"),
            layout,
            tv,
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
            theme: Memo::new(move |_| {
                themes.with(|themes| pick(themes, &theme_name.get(), "rose-pine").cloned())
            }),
            layout: Memo::new(move |_| {
                layouts.with(|layouts| pick(layouts, &layout_name.get(), "web").cloned())
            }),
            highlight: RwSignal::new(None),
            me: RwSignal::new(address.me.map(Login)),
            tv: address.tv,
        }
    }

    /// How far back the layout's history panels look, in seconds: the longest of them, or a day.
    pub fn history_span_s(&self) -> i64 {
        self.layout.with(|layout| {
            layout
                .as_ref()
                .and_then(|layout| longest_history(&layout.root))
                .unwrap_or(86_400)
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
