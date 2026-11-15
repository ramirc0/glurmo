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
    let rows = move || state.cluster.with(|cluster| rows(state, cluster, picks));
    view! {
        <section class="panel jobs">
            <h2>"jobs" <small>{counts}</small> <Controls picks=picks/></h2>
            <div class="scroll" node_ref=scroller>
                <table>
                    <thead>
                        <Headings order=picks.order/>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
            </div>
        </section>
    }
}

/// The search field and the filter chips. They are built once, so the field keeps focus as the list
/// changes.
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
    view! {
        <span class="controls">
            <input
                class="search"
                type="text"
                spellcheck="false"
                placeholder="user, job, name, node…"
                prop:value=move || picks.text.get()
                on:input=move |event| picks.text.set(event_target_value(&event))
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

/// A row per group, and under each open group a row per job.
fn rows(state: State, cluster: &Cluster, picks: Picks) -> Vec<AnyView> {
    let opened = picks.open.get();
    let highlight = state.row_highlight();
    let mut lines: Vec<AnyView> = Vec::new();
    for group in picks.groups(state, cluster) {
        if group.attempts.len() == 1 {
            lines.push(row(state, cluster, &group, highlight, None));
            continue;
        }
        let is_open = opened.contains(&group.key);
        lines.push(row(
            state,
            cluster,
            &group,
            highlight,
            Some((picks.open, is_open)),
        ));
        if !is_open {
            continue;
        }
        for attempt in group.attempts.iter().take(OPEN_ROWS) {
            let one = JobGroup {
                key: group.key.clone(),
                attempts: vec![*attempt],
            };
            lines.push(child(state, cluster, &one, highlight));
        }
        if group.attempts.len() > OPEN_ROWS {
            let more = format!(
                "{} more · narrow the search to see them",
                group.attempts.len() - OPEN_ROWS
            );
            lines.push(
                view! {
                    <tr class="child more">
                        <td colspan="11">{more}</td>
                    </tr>
                }
                .into_any(),
            );
        }
    }
    lines
}

/// A group's row, which opens and closes on a click when `toggle` is given, or a lone job's row.
/// It lights up by `highlight`.
fn row(
    state: State,
    cluster: &Cluster,
    group: &JobGroup,
    highlight: ReadSignal<Option<Highlight>>,
    toggle: Option<(RwSignal<BTreeSet<GroupKey>>, bool)>,
) -> AnyView {
    let key = group.key.clone();
    let click = toggle.map(|(open, _)| {
        Callback::new(move |()| {
            open.update(|open| {
                if !open.remove(&key) {
                    open.insert(key.clone());
                }
            });
        })
    });
    let class = match toggle {
        Some((_, true)) => "group open",
        Some((_, false)) => "group",
        None => "",
    };
    let count =
        toggle.map(|_| view! { <span class="count">{thousands(group.tasks())}</span> }.into_any());
    let body = cells(state, cluster, group, count);
    view! {
        <Lit members=Members::of(group) highlight=highlight class=class click=click>
            {body}
        </Lit>
    }
    .into_any()
}

/// One job in an open group. It lights up by `highlight`.
fn child(
    state: State,
    cluster: &Cluster,
    group: &JobGroup,
    highlight: ReadSignal<Option<Highlight>>,
) -> AnyView {
    let body = cells(state, cluster, group, None);
    view! {
        <Lit members=Members::of(group) highlight=highlight class="child" click=None>
            {body}
        </Lit>
    }
    .into_any()
}

/// The jobs a row stands for, and the nodes they run on.
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

/// A row that lights its jobs' cores on the Die while hovered, and lights itself while `highlight`, a
/// copy of the page's, is on one of its jobs or nodes. A click runs `click`.
#[component]
fn Lit(
    members: Members,
    highlight: ReadSignal<Option<Highlight>>,
    class: &'static str,
    click: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    let state = expect_context::<State>();
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

/// The cells of a row. `tasks` follows the label of a group of more than one.
fn cells(state: State, cluster: &Cluster, group: &JobGroup, tasks: Option<AnyView>) -> AnyView {
    let first: &Attempt = group.first();
    let at = cluster.at.unwrap_or_default();
    let running = group.key.phase == Phase::Running;
    let login = first.login.clone();
    let name = display_name(cluster, &login);
    let left = group.left_s(at).map_or_else(|| "–".to_owned(), span);
    let submitted = group
        .submitted()
        .map_or_else(String::new, |submit| clock(submit, true));
    let job_name = if first.name.is_empty() {
        "–".to_owned()
    } else {
        first.name.clone()
    };
    view! {
        <td class="name">
            <span
                class="state"
                class:pending=!running
                style:color=color_of(state, &login)
                style:background=running.then(|| color_of(state, &login))
            ></span>
            <span class="login">{login.0.clone()}</span>
            <span class="full">{name}</span>
        </td>
        <td class="name job">{group.label()} {tasks}</td>
        <td class="name title">{job_name}</td>
        <td class="name">{group.key.partitions.clone()}</td>
        <td>{thousands(group.cpus())}</td>
        <td class:dim=group.gpus() == 0>{count(group.gpus())}</td>
        <td>{group.mem_mb().map_or_else(|| "–".to_owned(), gigabytes)}</td>
        <td>{group.since().map_or_else(|| "–".to_owned(), |then| since(then, at))}</td>
        <td class="wide">{left}</td>
        <td class="wide">{submitted}</td>
        <td class="name where">{group.place()}</td>
    }
    .into_any()
}

/// Scrolls the list just enough to show its first lit row.
fn reveal_lit(scroller: NodeRef<leptos::html::Div>) {
    let Some(scroller) = scroller.get_untracked() else {
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
