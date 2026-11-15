//! The job list: running and pending jobs, grouped, with a search and filters. Hovering a row lights its
//! jobs' cores on the Die. Hovering a core on the Die lights its job's rows here, and hovering a node
//! lights the rows of every job on it. The list scrolls to the first.

use std::collections::BTreeSet;

use glurmo_model::jobs::{GroupKey, JobGroup, Order, Phase, Search, job_groups};
use glurmo_model::units::{count, gigabytes, since, span, thousands};
use glurmo_model::{Attempt, AttemptKey, Cluster, NodeName};
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::paint::{clock, display_name};
use crate::panels::color_of;
use crate::state::{Highlight, State};

/// The most attempts an open group lists.
const OPEN_ROWS: usize = 100;

/// How long the search waits after the last key before it filters, so typing never waits on the list.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// What the list's controls pick.
#[derive(Clone, Copy)]
struct Picks {
    text: RwSignal<String>,
    phase: RwSignal<Option<Phase>>,
    mine: RwSignal<bool>,
    gpus: RwSignal<bool>,
    order: RwSignal<Order>,
    open: RwSignal<BTreeSet<GroupKey>>,
}

impl Picks {
    fn new() -> Self {
        Self {
            text: RwSignal::new(String::new()),
            phase: RwSignal::new(None),
            mine: RwSignal::new(false),
            gpus: RwSignal::new(false),
            order: RwSignal::new(Order::default()),
            open: RwSignal::new(BTreeSet::new()),
        }
    }

    /// The groups the picks let through, in the picked order.
    fn groups(self, state: State, cluster: &Cluster) -> Vec<JobGroup<'_>> {
        let text = self.text.get();
        let me = state.me.get();
        let search = Search {
            text: &text,
            phase: self.phase.get(),
            login: me.as_ref().filter(|_| self.mine.get()),
            gpus: self.gpus.get(),
        };
        job_groups(cluster, &search, self.order.get())
    }
}

/// The job list panel.
#[component]
pub fn Jobs() -> impl IntoView {
    let state = expect_context::<State>();
    let picks = Picks::new();
    let scroller = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if matches!(
            state.highlight.get(),
            Some(Highlight::Job(_) | Highlight::Node(_))
        ) {
            request_animation_frame(move || reveal_lit(scroller));
        }
    });
    let counts = move || {
        state.cluster.with(|cluster| {
            let groups = picks.groups(state, cluster);
            let tasks = |which: Phase| -> u64 {
                groups
                    .iter()
                    .filter(|group| group.key.phase == which)
                    .map(JobGroup::tasks)
                    .sum()
            };
            format!(
                "{} running · {} pending",
                thousands(tasks(Phase::Running)),
                thousands(tasks(Phase::Pending))
            )
        })
    };
    let lines = move || state.cluster.with(|cluster| lines(state, cluster, picks));
    view! {
        <section class="panel jobs">
            <h2>"jobs" <small>{counts}</small> <Controls picks=picks/></h2>
            <div class="scroll" node_ref=scroller>
                <table>
                    <thead>
                        <Headings order=picks.order/>
                    </thead>
                    <tbody>
                        <For each=lines key=Line::clone children=move |line| line.view(picks.open)/>
                    </tbody>
                </table>
            </div>
        </section>
    }
}

/// The search field and the filter chips. They are built once, so the field keeps focus as the list
/// changes. The field holds its own text, and the list follows it once typing pauses.
#[component]
fn Controls(picks: Picks) -> impl IntoView {
    let state = expect_context::<State>();
    let chip = move |label: &'static str, on: Signal<bool>, pick: Callback<()>| {
        view! {
            <button class:on=move || on.get() on:click=move |_| pick.run(())>
                {label}
            </button>
        }
    };
    let phase = move |label: &'static str, which: Option<Phase>| {
        chip(
            label,
            Signal::derive(move || picks.phase.get() == which),
            Callback::new(move |()| picks.phase.set(which)),
        )
    };
    let flip = |flag: RwSignal<bool>| Callback::new(move |()| flag.update(|on| *on = !*on));
    let mut settle = debounce(SETTLE, move |text: String| {
        picks.text.try_set(text);
    });
    view! {
        <span class="controls">
            <input
                class="search"
                type="text"
                spellcheck="false"
                placeholder="user, job, name, node…"
                on:input=move |event| settle(event_target_value(&event))
            />
        </span>
        <span class="controls">
            {phase("all", None)}
            {phase("running", Some(Phase::Running))}
            {phase("pending", Some(Phase::Pending))}
        </span>
        <span class="controls">
            {move || state.me.get().is_some().then(|| chip("mine", picks.mine.into(), flip(picks.mine)))}
            {chip("gpu", picks.gpus.into(), flip(picks.gpus))}
        </span>
    }
}

/// The column headings. Those with an order sort the list on a click.
#[component]
fn Headings(order: RwSignal<Order>) -> impl IntoView {
    let heading = move |label: &'static str, by: Option<Order>, class: &'static str| {
        view! {
            <th
                class=class
                class:sortable=by.is_some()
                class:sorted=move || by.is_some_and(|by| order.get() == by)
                on:click=move |_| {
                    if let Some(by) = by {
                        order.set(by);
                    }
                }
            >
                {label}
            </th>
        }
    };
    view! {
        <tr>
            {heading("user", Some(Order::User), "name")}
            {heading("job", None, "name")}
            {heading("name", None, "name")}
            {heading("partition", None, "name")}
            {heading("cpus", Some(Order::Cpus), "")}
            {heading("gpus", Some(Order::Gpus), "")}
            {heading("mem", Some(Order::Memory), "")}
            {heading("time", Some(Order::Time), "")}
            {heading("left", None, "wide")}
            {heading("submitted", None, "wide")}
            {heading("where / why", None, "name")}
        </tr>
    }
}

/// A row of the list as it shows. The list keys its rows on their lines, so a change rebuilds only the
/// rows whose text, color, members or openness changed.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Line {
    /// A group, a lone job, or one job of an open group.
    Row(Box<Row>),
    /// How many jobs of an open group go unlisted.
    More { group: GroupKey, unlisted: usize },
}

/// A row's place in the list.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    /// A group of one job.
    Lone,
    /// A group of more than one, which opens and closes on a click.
    Group { open: bool, tasks: u64 },
    /// One job of an open group.
    Child,
}

/// The text of a row of jobs, worked out from the cluster, and the jobs it stands for.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Row {
    kind: Kind,
    group: GroupKey,
    members: Members,
    color: String,
    name: String,
    label: String,
    title: String,
    cpus: u64,
    gpus: u64,
    mem: String,
    since: String,
    left: String,
    submitted: String,
    place: String,
}

/// A line per group, and under each open group a line per job.
fn lines(state: State, cluster: &Cluster, picks: Picks) -> Vec<Line> {
    let opened = picks.open.get();
    let mut lines = Vec::new();
    for group in picks.groups(state, cluster) {
        if group.attempts.len() == 1 {
            lines.push(Line::Row(Box::new(Row::of(
                state,
                cluster,
                &group,
                Kind::Lone,
            ))));
            continue;
        }
        let open = opened.contains(&group.key);
        let kind = Kind::Group {
            open,
            tasks: group.tasks(),
        };
        lines.push(Line::Row(Box::new(Row::of(state, cluster, &group, kind))));
        if !open {
            continue;
        }
        for attempt in group.attempts.iter().take(OPEN_ROWS) {
            let one = JobGroup {
                key: group.key.clone(),
                attempts: vec![*attempt],
            };
            lines.push(Line::Row(Box::new(Row::of(
                state,
                cluster,
                &one,
                Kind::Child,
            ))));
        }
        if group.attempts.len() > OPEN_ROWS {
            lines.push(Line::More {
                group: group.key.clone(),
                unlisted: group.attempts.len() - OPEN_ROWS,
            });
        }
    }
    lines
}

impl Line {
    /// The table row. A group's row opens and closes it in `open` on a click.
    fn view(self, open: RwSignal<BTreeSet<GroupKey>>) -> AnyView {
        match self {
            Self::Row(row) => row.view(open),
            Self::More { unlisted, .. } => {
                let more = format!("{unlisted} more · narrow the search to see them");
                view! {
                    <tr class="child more">
                        <td colspan="11">{more}</td>
                    </tr>
                }
                .into_any()
            }
        }
    }
}

impl Row {
    fn of(state: State, cluster: &Cluster, group: &JobGroup, kind: Kind) -> Self {
        let first: &Attempt = group.first();
        let at = cluster.at.unwrap_or_default();
        Self {
            kind,
            group: group.key.clone(),
            members: Members::of(group),
            color: color_of(state, &group.key.login),
            name: display_name(cluster, &group.key.login),
            label: group.label(),
            title: if first.name.is_empty() {
                "–".to_owned()
            } else {
                first.name.clone()
            },
            cpus: group.cpus(),
            gpus: group.gpus(),
            mem: group.mem_mb().map_or_else(|| "–".to_owned(), gigabytes),
            since: group
                .since()
                .map_or_else(|| "–".to_owned(), |then| since(then, at)),
            left: group.left_s(at).map_or_else(|| "–".to_owned(), span),
            submitted: group
                .submitted()
                .map_or_else(String::new, |submit| clock(submit, true)),
            place: group.place(),
        }
    }

    fn view(self, open: RwSignal<BTreeSet<GroupKey>>) -> AnyView {
        let running = self.group.phase == Phase::Running;
        let (class, tasks) = match self.kind {
            Kind::Lone => ("", None),
            Kind::Child => ("child", None),
            Kind::Group { open: true, tasks } => ("group open", Some(tasks)),
            Kind::Group { open: false, tasks } => ("group", Some(tasks)),
        };
        let click = tasks.map(|_| {
            let key = self.group.clone();
            Callback::new(move |()| {
                open.update(|open| {
                    if !open.remove(&key) {
                        open.insert(key.clone());
                    }
                });
            })
        });
        let tasks = tasks.map(|tasks| view! { <span class="count">{thousands(tasks)}</span> });
        let color = self.color;
        view! {
            <Lit members=self.members class=class click=click>
                <td class="name">
                    <span
                        class="state"
                        class:pending=!running
                        style:color=color.clone()
                        style:background=running.then_some(color)
                    ></span>
                    <span class="login">{self.group.login.0.clone()}</span>
                    <span class="full">{self.name}</span>
                </td>
                <td class="name job">{self.label} {tasks}</td>
                <td class="name title">{self.title}</td>
                <td class="name">{self.group.partitions.clone()}</td>
                <td>{thousands(self.cpus)}</td>
                <td class:dim=self.gpus == 0>{count(self.gpus)}</td>
                <td>{self.mem}</td>
                <td>{self.since}</td>
                <td class="wide">{self.left}</td>
                <td class="wide">{self.submitted}</td>
                <td class="name where">{self.place}</td>
            </Lit>
        }
        .into_any()
    }
}

/// The jobs a row stands for, and the nodes they run on.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Members {
    keys: BTreeSet<AttemptKey>,
    nodes: BTreeSet<NodeName>,
}

impl Members {
    fn of(group: &JobGroup) -> Self {
        Self {
            keys: group.attempts.iter().map(|attempt| attempt.key).collect(),
            nodes: group
                .attempts
                .iter()
                .flat_map(|attempt| attempt.nodes.iter().cloned())
                .collect(),
        }
    }

    /// Whether the row lights up under `highlight`: its own, or one of its jobs or nodes under the
    /// Die's pointer.
    fn lit_by(&self, highlight: Option<&Highlight>) -> bool {
        match highlight {
            Some(Highlight::Job(key)) => self.keys.contains(key),
            Some(Highlight::Node(node)) => self.nodes.contains(node),
            Some(Highlight::Jobs(keys)) => *keys == self.keys,
            _ => false,
        }
    }
}

/// A row that lights its jobs' cores on the Die while hovered, and lights itself while the page's
/// highlight is on one of its jobs or nodes. It reads a copy of the highlight that dies with it. A click
/// runs `click`.
#[component]
fn Lit(
    members: Members,
    class: &'static str,
    click: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    let state = expect_context::<State>();
    let highlight = state.row_highlight();
    let members = StoredValue::new(members);
    let lit = move || {
        members.with_value(|members| highlight.with(|highlight| members.lit_by(highlight.as_ref())))
    };
    view! {
        <tr
            class=class
            class:lit=lit
            on:click=move |_| {
                if let Some(click) = click {
                    click.run(());
                }
            }
            on:mouseenter=move |_| {
                if !state.tv {
                    let keys = members.with_value(|members| members.keys.clone());
                    state.highlight.set(Some(Highlight::Jobs(keys)));
                }
            }
            on:mouseleave=move |_| {
                if !state.tv {
                    state.highlight.set(None);
                }
            }
        >
            {children()}
        </tr>
    }
}

/// Scrolls the list just enough to show its first lit row.
fn reveal_lit(scroller: NodeRef<leptos::html::Div>) {
    let Some(scroller) = scroller.try_get_untracked().flatten() else {
        return;
    };
    let Ok(Some(row)) = scroller.query_selector("tbody tr.lit") else {
        return;
    };
    let Ok(row) = row.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    let view = scroller.get_bounding_client_rect();
    let spot = row.get_bounding_client_rect();
    let head = scroller
        .query_selector("thead")
        .ok()
        .flatten()
        .map_or(0.0, |head| head.get_bounding_client_rect().height());
    let above = spot.top() - (view.top() + head);
    let below = spot.bottom() - view.bottom();
    let scroll = f64::from(scroller.scroll_top());
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a scroll offset fits in an i32"
    )]
    if above < 0.0 {
        scroller.set_scroll_top((scroll + above) as i32);
    } else if below > 0.0 {
        scroller.set_scroll_top((scroll + below) as i32);
    }
}
