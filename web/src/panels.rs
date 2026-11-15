//! The panels drawn as HTML: the header, tables of users, the queue, partitions, nodes and your jobs,
//! the ticker, and the key line.

use glurmo_model::layout::QueueBy;
use glurmo_model::summary::{self, Holding, holdings, partition_loads, waiting};
use glurmo_model::units::{percent, since, span, thousands};
use glurmo_model::{Attempt, Login, NodeName, NodeState, PartitionName};
use leptos::prelude::*;

use crate::paint::{Rgb, clock, display_name, now, user_color};
use crate::state::{Highlight, Link, State};

/// The CSS color of `login` in the current theme.
pub fn color_of(state: State, login: &Login) -> String {
    state.theme.with(|theme| {
        theme.as_ref().map_or_else(String::new, |theme| {
            state
                .cluster
                .with(|cluster| user_color(theme, cluster, login).css())
        })
    })
}

/// A bar filled to `part / whole`.
#[component]
fn Bar(part: u64, whole: u64, #[prop(into)] fill: String) -> impl IntoView {
    let width = percent(part, whole).min(100);
    view! {
        <span class="bar">
            <span class="fill" style:width=format!("{width}%") style:background=fill></span>
        </span>
    }
}

/// A table row that lights `highlight` on the Die while hovered, except on a wall display, where the
/// highlight cycles on its own. It shows as lit while its highlight is on.
#[component]
fn LitRow(highlight: Highlight, children: Children) -> impl IntoView {
    let state = expect_context::<State>();
    let lit = StoredValue::new(highlight);
    view! {
        <tr
            class:lit=move || lit.with_value(|lit| state.highlight.with(|on| on.as_ref() == Some(lit)))
            on:mouseenter=move |_| {
                if !state.tv {
                    state.highlight.set(Some(lit.get_value()));
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

/// The cluster, the sample time, cores and GPUs held, job counts, and trouble on the right.
#[component]
pub fn Header() -> impl IntoView {
    let state = expect_context::<State>();
    let totals = move || {
        state.cluster.with(|cluster| {
            state.occupancy.with(|occupancy| {
                let pending: u64 = cluster
                    .queue
                    .values()
                    .map(|attempt| u64::from(attempt.tasks()))
                    .sum();
                (
                    summary::cores(cluster, occupancy),
                    summary::gpus(cluster, occupancy),
                    cluster.running.len() as u64,
                    pending,
                )
            })
        })
    };
    let accent = move |slot: usize| {
        state.theme.with(|theme| {
            theme
                .as_ref()
                .and_then(|theme| theme.users.get(slot))
                .map_or_else(String::new, |color| Rgb::of(color).css())
        })
    };
    view! {
        <header class="panel header">
            <div class="brand">
                <span class="site">
                    {move || state.cluster_name.get()}
                    {move || {
                        state
                            .replay
                            .get()
                            .map(|playhead| {
                                let speed = playhead
                                    .speed
                                    .map_or_else(|| "paused".to_owned(), |speed| format!("{speed}×"));
                                view! { <span class="badge">{format!("replay · {speed}")}</span> }
                            })
                    }}
                </span>
                <span class="when">
                    {move || {
                        if let Some(playhead) = state.replay.get() {
                            clock(playhead.time(), true)
                        } else if state.cluster.with(|cluster| cluster.at.is_some()) {
                            clock(state.now.get(), true)
                        } else {
                            "waiting for the first sample".to_owned()
                        }
                    }}
                </span>
            </div>
            {move || {
                let (cores, gpus, running, pending) = totals();
                view! {
                    <div class="stat">
                        <span class="caption">"cores held"</span>
                        <span class="value">
                            {thousands(cores.held)}
                            <small>" / " {thousands(cores.total)}</small>
                        </span>
                        <Bar part=cores.held whole=cores.total fill=accent(0)/>
                    </div>
                    {(gpus.total > 0)
                        .then(|| {
                            view! {
                                <div class="stat">
                                    <span class="caption">"gpus held"</span>
                                    <span class="value">
                                        {gpus.held}
                                        <small>" / " {gpus.total}</small>
                                    </span>
                                    <Bar part=gpus.held whole=gpus.total fill=accent(1)/>
                                </div>
                            }
                        })}
                    <div class="stat">
                        <span class="caption">"running"</span>
                        <span class="value">{thousands(running)}</span>
                    </div>
                    <div class="stat">
                        <span class="caption">"pending tasks"</span>
                        <span class="value">{thousands(pending)}</span>
                    </div>
                }
            }}
            <Trouble/>
        </header>
    }
}

/// Why the page may be stale: no server, or no answers from Slurm.
#[component]
fn Trouble() -> impl IntoView {
    let state = expect_context::<State>();
    let trouble = move || {
        let link = state.link.get();
        let outage = state.cluster.with(|cluster| cluster.outage.clone());
        match (link, outage) {
            (Link::Connecting, _) => Some("connecting to the server".to_owned()),
            (Link::Lost(at_ms), _) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "milliseconds since 1970 fit in an i64"
                )]
                let at = glurmo_model::UnixTime((at_ms / 1_000.0) as i64);
                Some(format!(
                    "server unreachable for {}, reconnecting",
                    since(at, now())
                ))
            }
            (Link::Live, Some(outage)) => Some(format!(
                "no answer from Slurm for {}: {}",
                since(outage.since, now()),
                outage.error.lines().next().unwrap_or_default()
            )),
            (Link::Live, None) => None,
        }
    };
    move || trouble().map(|text| view! { <div class="trouble">{text}</div> })
}

/// One row per user, most cores first, and a row for cores the scheduler hides. Hovering a row
/// highlights that user's cores, except on a wall display, where the highlight cycles on its own.
#[component]
pub fn Users() -> impl IntoView {
    let state = expect_context::<State>();
    let rows = move || {
        state.cluster.with(|cluster| {
            state.occupancy.with(|occupancy| {
                let holdings = holdings(cluster, occupancy);
                let capacity = summary::cores(cluster, occupancy).total;
                let users: Vec<(Login, String, Holding)> = holdings
                    .users
                    .into_iter()
                    .map(|(login, held)| (login.clone(), display_name(cluster, login), held))
                    .collect();
                (users, holdings.hidden, capacity)
            })
        })
    };
    let share = move |held: &Holding, capacity: u64| {
        if held.cores == 0 {
            0
        } else {
            percent(held.cores, capacity).clamp(1, 100)
        }
    };
    view! {
        <section class="panel users">
            <h2>"users" <small>{move || format!("{} users", rows().0.len())}</small></h2>
            <table>
                <thead>
                    <tr>
                        <th></th>
                        <th class="name">"user"</th>
                        <th>"cores"</th>
                        <th>"gpus"</th>
                        <th>"jobs"</th>
                        <th>"pend"</th>
                        <th class="share">"share"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        let (users, hidden, capacity) = rows();
                        let me = state.me.get();
                        let mut lines: Vec<AnyView> = users
                            .into_iter()
                            .map(|(login, name, held)| {
                                let row = UserRow {
                                    you: me.as_ref() == Some(&login),
                                    share: share(&held, capacity),
                                    login,
                                    name,
                                    held,
                                };
                                row.view(state)
                            })
                            .collect();
                        if hidden.cores + hidden.gpus > 0 {
                            lines.push(
                                view! {
                                    <tr class="hidden">
                                        <td>
                                            <span class="swatch"></span>
                                        </td>
                                        <td class="name">"hidden"</td>
                                        <td>{thousands(hidden.cores)}</td>
                                        <td>{hidden.gpus}</td>
                                        <td></td>
                                        <td></td>
                                        <td></td>
                                    </tr>
                                }
                                    .into_any(),
                            );
                        }
                        lines
                    }}
                </tbody>
            </table>
        </section>
    }
}

/// One user's row in the users panel.
struct UserRow {
    login: Login,
    name: String,
    held: Holding,
    share: u64,
    you: bool,
}

impl UserRow {
    /// The row, which highlights the user's cores while hovered.
    fn view(self, state: State) -> AnyView {
        let color = color_of(state, &self.login);
        let held = self.held;
        view! {
            <LitRow highlight=Highlight::User(self.login)>
                <td>
                    <span class="swatch" style:background=color.clone()></span>
                </td>
                <td class="name">
                    {self.name}
                    {self.you.then(|| view! { <span class="you">"you"</span> })}
                </td>
                <td>{thousands(held.cores)}</td>
                <td>{held.gpus}</td>
                <td>{thousands(held.jobs)}</td>
                <td>{thousands(held.pending)}</td>
                <td class="share">
                    <span class="bar">
                        <span
                            class="fill"
                            style:width=format!("{}%", self.share)
                            style:background=color
                        ></span>
                    </span>
                </td>
            </LitRow>
        }
        .into_any()
    }
}

/// One row per partition or user, most tasks first, with the longest wait and the commonest reason.
#[component]
pub fn Queue(by: QueueBy) -> impl IntoView {
    let state = expect_context::<State>();
    let first = match by {
        QueueBy::Partition => "partition",
        QueueBy::Login => "user",
    };
    let rows = move || {
        state.cluster.with(|cluster| {
            let at = cluster.at.unwrap_or_default();
            waiting(cluster.queue.values(), by)
                .into_iter()
                .map(|row| {
                    let key = row.key.to_owned();
                    let wait = row
                        .oldest
                        .map_or_else(String::new, |oldest| since(oldest, at));
                    (
                        key,
                        row.jobs,
                        row.tasks,
                        row.cpus,
                        row.gpus,
                        wait,
                        row.reason.to_owned(),
                    )
                })
                .collect::<Vec<_>>()
        })
    };
    view! {
        <section class="panel queue">
            <h2>
                "queue"
                <small>
                    {move || {
                        format!("{} tasks", thousands(rows().iter().map(|row| row.2).sum()))
                    }}
                </small>
            </h2>
            <table>
                <thead>
                    <tr>
                        <th class="name">{first}</th>
                        <th>"jobs"</th>
                        <th>"tasks"</th>
                        <th>"cpus"</th>
                        <th>"gpus"</th>
                        <th>"wait"</th>
                        <th class="name">"why"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        rows()
                            .into_iter()
                            .map(|(key, jobs, tasks, cpus, gpus, wait, reason)| {
                                let (color, highlight) = match by {
                                    QueueBy::Login => {
                                        let login = Login(key.clone());
                                        (color_of(state, &login), Highlight::User(login))
                                    }
                                    QueueBy::Partition => {
                                        (String::new(), Highlight::Partition(PartitionName(key.clone())))
                                    }
                                };
                                view! {
                                    <LitRow highlight=highlight>
                                        <td class="name" style:color=color>{key}</td>
                                        <td>{thousands(jobs)}</td>
                                        <td>{thousands(tasks)}</td>
                                        <td>{thousands(cpus)}</td>
                                        <td>{thousands(gpus)}</td>
                                        <td>{wait}</td>
                                        <td class="name why">{reason}</td>
                                    </LitRow>
                                }
                            })
                            .collect_view()
                    }}
                </tbody>
            </table>
        </section>
    }
}

/// One row per partition, in the scheduler's order. The default partition is starred.
#[component]
pub fn Partitions() -> impl IntoView {
    let state = expect_context::<State>();
    view! {
        <section class="panel partitions">
            <h2>"partitions"</h2>
            <table>
                <thead>
                    <tr>
                        <th class="name">"partition"</th>
                        <th>"limit"</th>
                        <th>"run"</th>
                        <th>"pend"</th>
                        <th>"free cores"</th>
                        <th>"free gpus"</th>
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        state
                            .cluster
                            .with(|cluster| {
                                state
                                    .occupancy
                                    .with(|occupancy| {
                                        partition_loads(cluster, occupancy)
                                            .into_iter()
                                            .map(|load| {
                                                let partition = load.partition;
                                                let star = if partition.default { "*" } else { "" };
                                                let limit = partition
                                                    .max_time_s
                                                    .map_or_else(
                                                        || "none".to_owned(),
                                                        |limit| span(limit.cast_signed()),
                                                    );
                                                let name = format!("{}{star}", partition.name.0);
                                                let running = thousands(load.running);
                                                let pending = thousands(load.pending);
                                                let free_cores = thousands(load.free_cores);
                                                let free_gpus = (load.free_gpus > 0)
                                                    .then(|| load.free_gpus.to_string());
                                                view! {
                                                    <LitRow highlight=Highlight::Partition(partition.name.clone())>
                                                        <td class="name">{name}</td>
                                                        <td>{limit}</td>
                                                        <td>{running}</td>
                                                        <td>{pending}</td>
                                                        <td>{free_cores}</td>
                                                        <td>{free_gpus}</td>
                                                    </LitRow>
                                                }
                                            })
                                            .collect_view()
                                    })
                            })
                    }}
                </tbody>
            </table>
        </section>
    }
}

/// How many nodes are in each state, then each node that cannot take jobs and why.
#[component]
pub fn Nodes() -> impl IntoView {
    let state = expect_context::<State>();
    view! {
        <section class="panel nodes">
            <h2>"nodes"</h2>
            {move || {
                state
                    .cluster
                    .with(|cluster| {
                        let count = |wanted: NodeState| {
                            cluster
                                .nodes
                                .iter()
                                .filter(|node| node.state == wanted && node.trouble().is_none())
                                .count()
                        };
                        let out: Vec<_> = cluster
                            .nodes
                            .iter()
                            .filter_map(|node| Some((node, node.trouble()?)))
                            .map(|(node, word)| {
                                let drain = matches!(word, "draining" | "drained");
                                (node.name.0.clone(), word, drain, node.reason.clone().unwrap_or_default())
                            })
                            .collect();
                        view! {
                            <p class="counts">
                                <b>{count(NodeState::Idle)}</b>
                                " idle  "
                                <b>{count(NodeState::Mixed)}</b>
                                " mixed  "
                                <b>{count(NodeState::Allocated)}</b>
                                " full  "
                                <b class="down">{out.len()}</b>
                                " out"
                            </p>
                            <table>
                                <tbody>
                                    {out
                                        .into_iter()
                                        .map(|(name, word, drain, reason)| {
                                            view! {
                                                <LitRow highlight=Highlight::Node(NodeName(name.clone()))>
                                                    <td class="name">{name}</td>
                                                    <td class="name" class:drain=drain class:down=!drain>
                                                        {word}
                                                    </td>
                                                    <td class="name why">{reason}</td>
                                                </LitRow>
                                            }
                                        })
                                        .collect_view()}
                                </tbody>
                            </table>
                        }
                    })
            }}
        </section>
    }
}

/// Starts and ends as they happen, newest first. It shows no outcomes.
#[component]
pub fn Ticker() -> impl IntoView {
    let state = expect_context::<State>();
    view! {
        <section class="panel ticker">
            <h2>"ticker"</h2>
            <table>
                <tbody>
                    {move || {
                        state
                            .ticker
                            .with(|ticker| {
                                ticker
                                    .iter()
                                    .take(60)
                                    .map(|tick| {
                                        let color = color_of(state, &tick.login);
                                        let name = state
                                            .cluster
                                            .with(|cluster| display_name(cluster, &tick.login));
                                        let gpus = (tick.gpus > 0)
                                            .then(|| format!(" {} gpu", tick.gpus));
                                        let partition = tick
                                            .partition
                                            .as_ref()
                                            .map_or_else(String::new, |partition| partition.0.clone());
                                        view! {
                                            <tr class:ended=!tick.started>
                                                <td>{clock(tick.at, false)}</td>
                                                <td class="name">
                                                    {if tick.started { "▶ start" } else { "■ end" }}
                                                </td>
                                                <td class="name" style:color=color>{name}</td>
                                                <td>{format!("{} cpu", tick.cpus)} {gpus}</td>
                                                <td class="name why">{partition}</td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()
                            })
                    }}
                </tbody>
            </table>
        </section>
    }
}

/// The jobs of whoever the address names with `me`: running ones newest first, then pending ones.
#[component]
pub fn Mine() -> impl IntoView {
    let state = expect_context::<State>();
    let line = |attempt: &Attempt, state_word: &'static str, time: String, place: String| {
        let id = attempt.printed_id();
        let limit = attempt.time_limit_s.map_or_else(String::new, |limit| {
            format!("/{}", span(limit.cast_signed()))
        });
        let partition = attempt
            .partitions
            .first()
            .map_or_else(String::new, |partition| partition.0.clone());
        view! {
            <tr>
                <td class="name">{id}</td>
                <td class="name">{attempt.name.clone()}</td>
                <td class="name">{partition}</td>
                <td>{attempt.cpus}</td>
                <td>{attempt.gpus}</td>
                <td class="name">{state_word}</td>
                <td>{time + &limit}</td>
                <td class="name why">{place}</td>
            </tr>
        }
    };
    view! {
        <section class="panel mine">
            <h2>"your jobs"</h2>
            {move || {
                let Some(me) = state.me.get() else {
                    return view! { <p class="note">"Add ?me=LOGIN to the address to list your jobs."</p> }
                        .into_any();
                };
                state
                    .cluster
                    .with(|cluster| {
                        let at = cluster.at.unwrap_or_default();
                        let mut running: Vec<&Attempt> = cluster
                            .running
                            .values()
                            .map(|running| &running.attempt)
                            .filter(|attempt| attempt.login == me)
                            .collect();
                        running.sort_by_key(|attempt| std::cmp::Reverse(attempt.start));
                        let mut lines: Vec<AnyView> = running
                            .into_iter()
                            .map(|attempt| {
                                let time = attempt.start.map_or_else(String::new, |start| since(start, at));
                                let nodes = attempt
                                    .nodes
                                    .iter()
                                    .map(|node| node.0.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",");
                                line(attempt, "run", time, nodes).into_any()
                            })
                            .collect();
                        lines.extend(
                            cluster
                                .queue
                                .values()
                                .filter(|attempt| attempt.login == me)
                                .map(|attempt| {
                                    let time = since(attempt.key.submit, at);
                                    let reason = attempt.reason.clone().unwrap_or_default();
                                    line(attempt, "pend", time, reason).into_any()
                                }),
                        );
                        view! {
                            <table>
                                <tbody>{lines}</tbody>
                            </table>
                        }
                            .into_any()
                    })
            }}
        </section>
    }
}

/// The keys the page answers to.
#[component]
pub fn Keys() -> impl IntoView {
    view! {
        <footer class="panel keys">
            <span>
                <kbd>"t"</kbd>
                " theme"
            </span>
            <span>
                <kbd>"l"</kbd>
                " layout"
            </span>
            <span>
                <kbd>"e"</kbd>
                " edit"
            </span>
            <span>
                <kbd>"f"</kbd>
                " fill the screen"
            </span>
            <span>"hover a row to light up its cores, or a core for its job"</span>
        </footer>
    }
}
