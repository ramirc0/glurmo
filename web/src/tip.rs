//! The Die's tooltip: the job that holds a core or GPU, or the node and who holds its cores.

use std::collections::BTreeMap;

use leptos::prelude::*;
use rtviz_model::units::{gigabytes, load, since, span};
use rtviz_model::{Attempt, Cluster, Node, Occupancies, Occupancy, Owner};

use crate::die::{Spot, owner_at};
use crate::paint::display_name;

/// The most holders a node's tooltip lists.
const HOLDERS: usize = 6;

/// A tooltip: a heading, labeled lines, and where it sits in its panel.
#[derive(Clone, Debug, PartialEq)]
pub struct Tip {
    head: String,
    rows: Vec<(String, String)>,
    place: String,
}

impl Tip {
    /// What to say about `spot` on `node`.
    pub fn of(cluster: &Cluster, occupancy: &Occupancies, node: &Node, spot: Spot) -> Self {
        let cells = occupancy.get(&node.name);
        let what = match spot {
            Spot::Core(i) => format!(" · core {i}"),
            Spot::Gpu(i) => format!(" · gpu {i}"),
            Spot::Tile => String::new(),
        };
        let rows = match owner_at(occupancy, node, spot) {
            Some(Owner::Attempt(key)) => cluster.running.get(&key).map_or_else(
                || vec![row("job", "ended")],
                |running| job(cluster, cells, &running.attempt, Owner::Attempt(key)),
            ),
            Some(Owner::Hidden) => vec![row("job", "hidden by the scheduler")],
            None => self::node(cluster, cells, node),
        };
        Self {
            head: format!("{}{what}", node.name.0),
            rows,
            place: String::new(),
        }
    }

    /// The tip beside the pointer at `(x, y)`, opening toward the roomier side of a `width` by `height`
    /// panel so it stays inside.
    pub fn at(mut self, x: f64, y: f64, width: f64, height: f64) -> Self {
        let across = if x < width / 2.0 {
            format!("left: {}px", x + 14.0)
        } else {
            format!("right: {}px", width - x + 14.0)
        };
        let down = if y < height / 2.0 {
            format!("top: {}px", y + 14.0)
        } else {
            format!("bottom: {}px", height - y + 14.0)
        };
        self.place = format!("{across}; {down}");
        self
    }

    /// The tip as HTML.
    pub fn view(self) -> impl IntoView {
        let rows = self
            .rows
            .into_iter()
            .map(|(label, value)| {
                view! {
                    <div class="row">
                        <span>{label}</span>
                        <span>{value}</span>
                    </div>
                }
            })
            .collect_view();
        view! {
            <div class="tip" style=self.place>
                <div class="head">{self.head}</div>
                {rows}
            </div>
        }
    }
}

fn row(label: &str, value: impl Into<String>) -> (String, String) {
    (label.to_owned(), value.into())
}

/// The job `attempt`, which holds cells on this node as `owner`.
fn job(
    cluster: &Cluster,
    cells: Option<&Occupancy>,
    attempt: &Attempt,
    owner: Owner,
) -> Vec<(String, String)> {
    let at = cluster.at.unwrap_or_default();
    let here = cells.map_or(0, |cells| {
        cells
            .cores
            .iter()
            .filter(|cell| **cell == Some(owner))
            .count()
    });
    let mut rows = vec![
        row("job", attempt.printed_id()),
        row("user", display_name(cluster, &attempt.login)),
    ];
    if let Some(partition) = attempt.partitions.first() {
        rows.push(row("partition", partition.0.clone()));
    }
    rows.push(row("cores here", here.to_string()));
    if attempt.nodes.len() > 1 {
        rows.push(row("nodes", attempt.nodes.len().to_string()));
    }
    if attempt.gpus > 0 {
        rows.push(row("gpus", attempt.gpus.to_string()));
    }
    if let Some(start) = attempt.start {
        rows.push(row("running", since(start, at)));
        if let Some(limit) = attempt.time_limit_s {
            rows.push(row("left", span(start.0 + limit.cast_signed() - at.0)));
        }
    }
    rows
}

/// The node's state, load and memory, why it is out, and who holds the most of its cores.
fn node(cluster: &Cluster, cells: Option<&Occupancy>, node: &Node) -> Vec<(String, String)> {
    let mut rows = vec![row("state", node.state_raw.to_lowercase())];
    if let Some(value) = node.load {
        rows.push(row("load", load(value)));
    }
    if let Some(free) = node.mem_free_mb {
        rows.push(row("mem free", gigabytes(free)));
    }
    if let Some(reason) = &node.reason {
        rows.push(row("reason", reason.clone()));
    }
    let mut held: BTreeMap<String, usize> = BTreeMap::new();
    for owner in cells.iter().flat_map(|cells| cells.cores.iter().flatten()) {
        let holder = match owner {
            Owner::Attempt(key) => cluster.running.get(key).map_or_else(
                || "ended".to_owned(),
                |running| display_name(cluster, &running.attempt.login),
            ),
            Owner::Hidden => "hidden".to_owned(),
        };
        *held.entry(holder).or_default() += 1;
    }
    let mut held: Vec<_> = held.into_iter().collect();
    held.sort_by_key(|(_, cores)| std::cmp::Reverse(*cores));
    rows.extend(
        held.into_iter()
            .take(HOLDERS)
            .map(|(holder, cores)| (holder, format!("{cores} cores"))),
    );
    rows
}
