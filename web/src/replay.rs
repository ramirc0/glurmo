//! Replays history: the cluster at a past moment, then its changes at a chosen speed. The live stream
//! carries on underneath, set aside, so going back to live is instant.

use std::collections::{BTreeMap, HashMap, VecDeque};

use glurmo_model::summary::Ticker;
use glurmo_model::{
    AttemptKey, Cluster, Delta, Event, Login, NodeName, Occupancies, Owner, UnixTime,
};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::feed::fetch;
use crate::state::State;

/// The speeds offered, in seconds of history per second.
pub const SPEEDS: [u32; 4] = [1, 10, 60, 600];

/// How long a start flashes and an end glows, in seconds of history, as in the mockup.
const FLASH_S: f64 = 45.0;
const AFTERGLOW_S: f64 = 150.0;

/// A replay this close to now has caught up, and the live view takes over.
const CAUGHT_UP_S: f64 = 15.0;

/// Wall seconds of history to fetch ahead of the playhead, within five minutes to an hour of history.
const AHEAD_S: f64 = 120.0;
const WINDOW_S: (f64, f64) = (300.0, 3_600.0);

/// Where a replay is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playhead {
    /// The moment shown, in seconds since the epoch.
    pub at: f64,
    /// Seconds of history per second, or `None` while paused.
    pub speed: Option<u32>,
}

impl Playhead {
    /// The moment shown, to the second.
    pub fn time(self) -> UnixTime {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "seconds since 1970 fit in an i64"
        )]
        let seconds = self.at.floor() as i64;
        UnixTime(seconds)
    }
}

/// A core or GPU on a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cell {
    /// A core, by index.
    Core(usize),
    /// A GPU, by index.
    Gpu(usize),
}

/// A cell that changed hands, which the Die lights for a while: a flash when a start takes it, and the
/// old holder's color fading when an end frees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    /// Whether a start took the cell, rather than an end freeing it.
    pub started: bool,
    /// When, in seconds since the epoch.
    pub at: f64,
    /// Who took or freed it.
    pub login: Login,
}

impl Effect {
    /// How strongly it shows with the playhead at `now`: 1 when fresh, falling to 0, or `None` when over.
    pub fn strength(&self, now: f64) -> Option<f64> {
        let span = if self.started { FLASH_S } else { AFTERGLOW_S };
        let age = now - self.at;
        (0.0..=span).contains(&age).then(|| 1.0 - age / span)
    }
}

/// Cells that just changed hands, by node.
pub type Effects = BTreeMap<NodeName, HashMap<Cell, Effect>>;

/// The live view, kept current while a replay has the screen.
struct Aside {
    cluster: Cluster,
    ticker: Ticker,
}

/// What a replay keeps between frames.
#[derive(Default)]
pub struct Engine {
    /// Counts seeks, so answers to an old one are dropped.
    generation: u64,
    /// The live view while a replay has the screen.
    aside: Option<Aside>,
    /// Fetched changes the playhead has not reached.
    deltas: VecDeque<Delta>,
    /// How far the fetched changes reach, in seconds since the epoch.
    fetched_to: f64,
    /// Whether the cluster at the playhead is on its way, which holds the playhead.
    loading: bool,
    /// Whether a window of changes is on its way.
    fetching: bool,
    /// Whether frames are running.
    animating: bool,
    /// When the last frame ran, in milliseconds since the epoch.
    last_frame_ms: Option<f64>,
    /// Cells that just changed hands, by node.
    pub effects: Effects,
}

impl Engine {
    /// Applies `change` to the live view a replay set aside, or hands it back when none is.
    fn change_aside<F: FnOnce(&mut Cluster, &mut Ticker)>(&mut self, change: F) -> Option<F> {
        let Some(aside) = &mut self.aside else {
            return Some(change);
        };
        change(&mut aside.cluster, &mut aside.ticker);
        None
    }
}

/// Changes the live view: the one set aside while a replay has the screen, or else the one shown.
pub fn update_live(state: State, change: impl FnOnce(&mut Cluster, &mut Ticker)) {
    let mut unapplied = None;
    state
        .engine
        .update_value(|engine| unapplied = engine.change_aside(change));
    if let Some(change) = unapplied {
        state
            .ticker
            .update(|ticker| state.cluster.update(|cluster| change(cluster, ticker)));
    }
}

/// Shows the cluster at `at`, in seconds since the epoch, setting the live view aside the first time.
/// A replay already playing keeps its speed.
pub fn seek(state: State, at: f64) {
    let generation = state.engine.with_value(|engine| engine.generation) + 1;
    let aside = Aside {
        cluster: state.cluster.get_untracked(),
        ticker: state.ticker.get_untracked(),
    };
    state.engine.update_value(|engine| {
        engine.aside.get_or_insert(aside);
        engine.generation = generation;
        engine.deltas.clear();
        engine.effects.clear();
        engine.fetched_to = at.floor();
        engine.loading = true;
        engine.fetching = false;
        engine.last_frame_ms = None;
    });
    let speed = state
        .replay
        .get_untracked()
        .and_then(|playhead| playhead.speed);
    state.replay.set(Some(Playhead { at, speed }));
    let time = Playhead { at, speed }.time().0;
    spawn_local(async move {
        let past = fetch::<Cluster>(&format!("/api/past?at={time}")).await;
        if !current(state, generation) {
            return;
        }
        match past {
            Ok(cluster) => {
                state.cluster.try_set(cluster);
                state.ticker.try_set(Ticker::default());
                state.engine.update_value(|engine| engine.loading = false);
            }
            Err(error) => {
                web_sys::console::warn_1(&format!("cannot replay: {error}").into());
                live(state);
            }
        }
    });
    animate(state);
}

/// Plays at `speed`, or pauses at `None`.
pub fn play(state: State, speed: Option<u32>) {
    state
        .engine
        .update_value(|engine| engine.last_frame_ms = None);
    state.replay.update(|playhead| {
        if let Some(playhead) = playhead {
            playhead.speed = speed;
        }
    });
}

/// Goes back to the live view.
pub fn live(state: State) {
    let mut aside = None;
    state.engine.update_value(|engine| {
        engine.generation += 1;
        engine.deltas.clear();
        engine.effects.clear();
        engine.loading = false;
        engine.fetching = false;
        aside = engine.aside.take();
    });
    if let Some(aside) = aside {
        state.cluster.try_set(aside.cluster);
        state.ticker.try_set(aside.ticker);
    }
    state.replay.try_set(None);
}

/// A time as fractional seconds since the epoch, as the playhead counts them.
fn seconds(time: UnixTime) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "seconds since 1970 fit in an f64"
    )]
    let seconds = time.0 as f64;
    seconds
}

fn current(state: State, generation: u64) -> bool {
    state
        .engine
        .try_with_value(|engine| engine.generation == generation)
        .unwrap_or(false)
}

/// Runs a frame each time the browser paints, while a replay lasts.
fn animate(state: State) {
    let started = state
        .engine
        .try_update_value(|engine| !std::mem::replace(&mut engine.animating, true))
        .unwrap_or(false);
    if started {
        request_animation_frame(move || frame(state));
    }
}

fn frame(state: State) {
    if state.replay.try_get_untracked().flatten().is_none() {
        state.engine.update_value(|engine| engine.animating = false);
        return;
    }
    advance(state);
    request_animation_frame(move || frame(state));
}

/// Moves the playhead on by the time since the last frame, applies the changes it passed, and fetches
/// more ahead of it.
fn advance(state: State) {
    let Some(mut playhead) = state.replay.get_untracked() else {
        return;
    };
    let now_ms = js_sys::Date::now();
    let mut elapsed_s = 0.0;
    let mut held = false;
    let mut fetched_to = 0.0;
    state.engine.update_value(|engine| {
        elapsed_s = engine
            .last_frame_ms
            .map_or(0.0, |last| (now_ms - last) / 1_000.0);
        engine.last_frame_ms = Some(now_ms);
        held = engine.loading;
        fetched_to = engine.fetched_to;
    });
    if let Some(speed) = playhead.speed.filter(|_| !held) {
        let next = (playhead.at + elapsed_s * f64::from(speed)).min(fetched_to);
        if next >= now_ms / 1_000.0 - CAUGHT_UP_S {
            live(state);
            return;
        }
        if next > playhead.at {
            playhead.at = next;
            state.replay.set(Some(playhead));
        }
    }
    let mut due = Vec::new();
    state.engine.update_value(|engine| {
        while engine
            .deltas
            .front()
            .is_some_and(|delta| seconds(delta.at) <= playhead.at)
        {
            due.extend(engine.deltas.pop_front());
        }
    });
    if !due.is_empty() {
        apply(state, &due);
    }
    if let Some(speed) = playhead.speed {
        fetch_ahead(state, playhead.at, speed, now_ms / 1_000.0);
    }
}

/// Applies `due` to the cluster shown, with its ticker, and lights the cells that changed hands.
fn apply(state: State, due: &[Delta]) {
    let mut taken: HashMap<AttemptKey, (f64, Login)> = HashMap::new();
    let mut freed: HashMap<AttemptKey, (f64, Login)> = HashMap::new();
    let mut before = Occupancies::new();
    let mut after = Occupancies::new();
    state.cluster.update(|cluster| {
        before = cluster.occupancy();
        for delta in due {
            let at = seconds(delta.at);
            for event in &delta.events {
                match event {
                    Event::Started { attempt } => {
                        taken.insert(attempt.key, (at, attempt.login.clone()));
                    }
                    Event::Ended { key, .. } => {
                        if let Some(running) = cluster.running.get(key) {
                            freed.insert(*key, (at, running.attempt.login.clone()));
                        }
                    }
                    _ => {}
                }
            }
            state.ticker.update(|ticker| ticker.record(cluster, delta));
            cluster.apply(delta);
        }
        cluster.tally_allocation();
        after = cluster.occupancy();
    });
    let now = due.last().map_or(0.0, |delta| seconds(delta.at));
    state.engine.update_value(|engine| {
        let mut light =
            |occupancy: &Occupancies, held: &HashMap<AttemptKey, (f64, Login)>, started| {
                for (node, cells) in occupancy {
                    let cores = cells
                        .cores
                        .iter()
                        .enumerate()
                        .map(|(i, owner)| (Cell::Core(i), owner));
                    let gpus = cells
                        .gpus
                        .iter()
                        .enumerate()
                        .map(|(i, owner)| (Cell::Gpu(i), owner));
                    for (cell, owner) in cores.chain(gpus) {
                        if let Some(Owner::Attempt(key)) = owner
                            && let Some((at, login)) = held.get(key)
                        {
                            let effect = Effect {
                                started,
                                at: *at,
                                login: login.clone(),
                            };
                            engine
                                .effects
                                .entry(node.clone())
                                .or_default()
                                .insert(cell, effect);
                        }
                    }
                }
            };
        light(&before, &freed, false);
        light(&after, &taken, true);
        for cells in engine.effects.values_mut() {
            cells.retain(|_, effect| effect.strength(now).is_some());
        }
        engine.effects.retain(|_, cells| !cells.is_empty());
    });
}

/// Fetches the next window of changes when the playhead nears the end of those fetched.
fn fetch_ahead(state: State, at: f64, speed: u32, now: f64) {
    let window = (AHEAD_S * f64::from(speed)).clamp(WINDOW_S.0, WINDOW_S.1);
    let mut next = None;
    state.engine.update_value(|engine| {
        if engine.loading || engine.fetching || engine.fetched_to >= now {
            return;
        }
        if engine.fetched_to - at < window / 2.0 {
            engine.fetching = true;
            next = Some((
                engine.generation,
                engine.fetched_to,
                engine.fetched_to + window,
            ));
        }
    });
    let Some((generation, from, to)) = next else {
        return;
    };
    spawn_local(async move {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "seconds since 1970 fit in an i64"
        )]
        let url = format!(
            "/api/past/events?from={}&to={}",
            from.floor() as i64,
            to.floor() as i64
        );
        let deltas = fetch::<Vec<Delta>>(&url).await;
        if !current(state, generation) {
            return;
        }
        match deltas {
            Ok(deltas) => state.engine.update_value(|engine| {
                engine.deltas.extend(deltas);
                engine.fetched_to = to.floor();
                engine.fetching = false;
            }),
            Err(error) => {
                web_sys::console::warn_1(&format!("cannot replay further: {error}").into());
                live(state);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_ten(cluster: &mut Cluster, _: &mut Ticker) {
        cluster.at = Some(UnixTime(10));
    }

    #[test]
    fn live_changes_go_aside_only_while_a_replay_holds_the_screen() {
        let mut engine = Engine::default();
        assert!(
            engine.change_aside(note_ten).is_some(),
            "with nothing aside, the change comes back for the screen"
        );
        engine.aside = Some(Aside {
            cluster: Cluster::default(),
            ticker: Ticker::default(),
        });
        assert!(engine.change_aside(note_ten).is_none());
        assert_eq!(
            engine.aside.map(|aside| aside.cluster.at),
            Some(Some(UnixTime(10)))
        );
    }
}
