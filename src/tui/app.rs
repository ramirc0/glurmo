//! The terminal view's state: the cluster, what the user picked, and what the feed reported.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::tz::TimeZone;

use super::feed::{Link, Update, Usage};
use super::view;
use crate::config::Tui;
use crate::hub::Message;
use crate::layout::{Layout, Pane};
use crate::library::Library;
use crate::model::{Attempt, Cluster, Delta, Event, Login, PartitionName, UnixTime};
use crate::slurm::Site;
use crate::theme::Theme;

/// Ticker lines kept.
const TICKER: usize = 200;

/// An attempt taking or freeing its cores, for the ticker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    /// When it started or ended.
    pub at: UnixTime,
    /// Whether it started, as opposed to ended.
    pub started: bool,
    /// The owner.
    pub login: Login,
    /// The partition it runs in.
    pub partition: Option<PartitionName>,
    /// CPUs it holds.
    pub cpus: u32,
    /// GPUs it holds.
    pub gpus: u32,
}

impl Tick {
    fn of(attempt: &Attempt, at: UnixTime, started: bool) -> Self {
        Self {
            at,
            started,
            login: attempt.login.clone(),
            partition: attempt.partitions.first().cloned(),
            cpus: attempt.cpus,
            gpus: attempt.gpus,
        }
    }
}

/// Text that narrows what the panels show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// The text, matched without regard to case.
    pub text: String,
    /// Whether keys go to the text.
    pub editing: bool,
}

/// Everything the view draws.
pub struct App {
    /// The cluster as the feed built it up.
    pub cluster: Cluster,
    /// Site facts, once a snapshot arrived.
    pub site: Option<Site>,
    /// Themes and layouts.
    pub library: Library,
    /// The layout shown.
    pub layout: String,
    /// The theme drawn with.
    pub theme: String,
    /// Whoever runs the view.
    pub me: Login,
    /// The filter.
    pub filter: Filter,
    /// Whether the key help shows.
    pub help: bool,
    /// The panel that takes scroll keys, counted in layout order.
    pub focus: usize,
    /// How far each panel is scrolled.
    pub scrolls: Vec<u16>,
    /// How far each panel could scroll, as of the last frame.
    pub limits: Vec<u16>,
    /// The connection to the server.
    pub link: Link,
    /// Usage history, or why it could not be read.
    pub usage: Result<Usage, String>,
    /// Recent starts and ends, latest first.
    pub ticker: VecDeque<Tick>,
    /// The time zone for clocks.
    pub tz: TimeZone,
    /// Whether the user asked to quit.
    pub quit: bool,
}

impl App {
    /// A view of nothing yet, for `me`, with the layout and theme `tui` names.
    pub fn new(library: Library, tui: &Tui, me: Login, tz: TimeZone) -> Self {
        Self {
            cluster: Cluster::default(),
            site: None,
            library,
            layout: tui.layout.clone(),
            theme: tui.theme.clone(),
            me,
            filter: Filter::default(),
            help: false,
            focus: 0,
            scrolls: Vec::new(),
            limits: Vec::new(),
            link: Link::Connecting,
            usage: Ok(Usage::default()),
            ticker: VecDeque::new(),
            tz,
            quit: false,
        }
    }

    /// The layout shown: the one picked, else `tui`, else the first there is.
    pub fn layout(&self) -> &Layout {
        let layouts = &self.library.layouts;
        layouts
            .get(&self.layout)
            .or_else(|| layouts.get("tui"))
            .or_else(|| layouts.values().next())
            .expect("the built-in layouts are always there")
    }

    /// The theme drawn with: the one picked, else `rose-pine`, else the first there is.
    pub fn theme(&self) -> &Theme {
        let themes = &self.library.themes;
        themes
            .get(&self.theme)
            .or_else(|| themes.get("rose-pine"))
            .or_else(|| themes.values().next())
            .expect("the built-in themes are always there")
    }

    /// How far back the layout's history panels look, in seconds: the longest of them, or a day.
    pub fn history_span_s(&self) -> i64 {
        view::panels(&self.layout().root)
            .into_iter()
            .filter_map(|pane| match pane {
                Pane::History { span } => Some(span.seconds()),
                _ => None,
            })
            .max()
            .unwrap_or(86_400)
    }

    /// The filter text, if one is set.
    pub fn filter(&self) -> Option<&str> {
        Some(self.filter.text.as_str()).filter(|text| !text.is_empty())
    }

    /// Takes in what the feed sent.
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Message(Message::Snapshot(snapshot)) => {
                self.cluster = snapshot.cluster;
                self.site = Some(snapshot.site);
            }
            Update::Message(Message::Delta(delta)) => self.apply(&delta),
            Update::Link(link) => self.link = link,
            Update::Usage(usage) => self.usage = usage,
        }
    }

    /// Applies a delta and adds its starts and ends to the ticker, latest first.
    fn apply(&mut self, delta: &Delta) {
        let mut started: HashMap<_, &Attempt> = HashMap::new();
        for event in &delta.events {
            match event {
                Event::Started { attempt } => {
                    started.insert(attempt.key, attempt);
                    let at = attempt.start.unwrap_or(delta.at);
                    self.ticker.push_front(Tick::of(attempt, at, true));
                }
                Event::Ended { key, at, .. } => {
                    let attempt = started.get(key).copied().or_else(|| {
                        self.cluster
                            .running
                            .get(key)
                            .map(|running| &running.attempt)
                    });
                    if let Some(attempt) = attempt {
                        self.ticker.push_front(Tick::of(attempt, *at, false));
                    }
                }
                _ => {}
            }
        }
        self.ticker
            .make_contiguous()
            .sort_by_key(|tick| std::cmp::Reverse(tick.at));
        self.ticker.truncate(TICKER);
        self.cluster.apply(delta);
    }

    /// Acts on a key.
    pub fn key(&mut self, key: KeyEvent) {
        if self.filter.editing {
            match key.code {
                KeyCode::Enter => self.filter.editing = false,
                KeyCode::Esc => self.filter = Filter::default(),
                KeyCode::Backspace => {
                    self.filter.text.pop();
                }
                KeyCode::Char(c) => self.filter.text.push(c),
                _ => {}
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let panels = self.limits.len().max(1);
        match key.code {
            KeyCode::Esc if self.filter().is_some() => self.filter = Filter::default(),
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Tab => self.focus = (self.focus + 1) % panels,
            KeyCode::BackTab => self.focus = (self.focus + panels - 1) % panels,
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::PageDown => self.scroll(10),
            KeyCode::PageUp => self.scroll(-10),
            KeyCode::Home | KeyCode::Char('g') => self.scroll(i32::from(i16::MIN)),
            KeyCode::Char('/') => self.filter.editing = true,
            KeyCode::Char('m') => {
                self.filter.text = if self.filter.text == self.me.0 {
                    String::new()
                } else {
                    self.me.0.clone()
                };
            }
            KeyCode::Char('l') => {
                self.layout = next(&self.library.layouts, &self.layout);
                self.focus = 0;
                self.scrolls.clear();
            }
            KeyCode::Char('t') => self.theme = next(&self.library.themes, &self.theme),
            KeyCode::Char('?') => self.help = true,
            _ => {}
        }
    }

    /// Scrolls the focused panel by `lines`, within what the last frame allowed.
    fn scroll(&mut self, lines: i32) {
        let limit = self.limits.get(self.focus).copied().unwrap_or(0);
        if self.scrolls.len() <= self.focus {
            self.scrolls.resize(self.focus + 1, 0);
        }
        let scroll = &mut self.scrolls[self.focus];
        let moved = (i32::from(*scroll) + lines).clamp(0, i32::from(limit));
        *scroll = u16::try_from(moved).unwrap_or(0);
    }

    /// How far panel `index` is scrolled.
    pub fn scroll_of(&self, index: usize) -> u16 {
        self.scrolls.get(index).copied().unwrap_or(0)
    }
}

/// The name after `current` in `names`, wrapping around.
fn next<T>(names: &BTreeMap<String, T>, current: &str) -> String {
    let mut after = names.keys().skip_while(|name| *name != current).skip(1);
    after
        .next()
        .or_else(|| names.keys().next())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::model::{AttemptKey, JobId, JobState};

    fn app() -> App {
        App::new(
            Library::load(Path::new("/nonexistent")),
            &Tui::default(),
            Login("otter".into()),
            TimeZone::UTC,
        )
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::from(code));
    }

    /// Attempt `id` holds `id` CPUs and started at `id`.
    fn attempt(id: u32) -> Attempt {
        Attempt {
            key: AttemptKey {
                job: JobId(id),
                submit: UnixTime(0),
            },
            array: None,
            login: Login("heron".into()),
            partitions: vec![PartitionName("4hours".into())],
            state: JobState::Running,
            state_raw: "RUNNING".into(),
            cpus: id,
            mem_mb: None,
            gpus: 0,
            time_limit_s: None,
            start: Some(UnixTime(id.into())),
            end: None,
            nodes: Vec::new(),
            reason: None,
            name: "job".into(),
        }
    }

    fn delta(seq: u64, events: Vec<Event>) -> Update {
        Update::Message(Message::Delta(Arc::new(Delta {
            seq,
            at: UnixTime(i64::try_from(seq).unwrap() * 10),
            events,
        })))
    }

    #[test]
    fn the_ticker_names_who_started_and_ended_even_within_one_delta() {
        let mut app = app();
        let ended = |id, at| Event::Ended {
            key: attempt(id).key,
            at: UnixTime(at),
            state: None,
        };
        app.update(delta(
            1,
            vec![Event::Started {
                attempt: attempt(1),
            }],
        ));
        app.update(delta(
            2,
            vec![
                Event::Started {
                    attempt: attempt(2),
                },
                ended(2, 14),
                ended(1, 15),
                ended(9, 16),
            ],
        ));
        let ticks: Vec<(u32, bool, UnixTime)> = app
            .ticker
            .iter()
            .map(|t| (t.cpus, t.started, t.at))
            .collect();
        assert_eq!(
            ticks,
            [
                (1, false, UnixTime(15)),
                (2, false, UnixTime(14)),
                (2, true, UnixTime(2)),
                (1, true, UnixTime(1)),
            ],
            "latest first, and an end with no known start is left out"
        );
        assert!(app.cluster.running.is_empty());
    }

    #[test]
    fn keys_cycle_layouts_and_themes_and_edit_the_filter() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.layout, "tv");
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.layout, "tui");
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.theme, "rose-pine-dawn");

        press(&mut app, KeyCode::Char('/'));
        for c in "4hq".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.filter(), Some("4h"));
        assert!(!app.quit);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.filter(), None);
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.filter(), Some("otter"));
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.filter(), None);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn scrolling_stays_within_the_last_frame() {
        let mut app = app();
        app.limits = vec![0, 3];
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.scroll_of(1), 3);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.scroll_of(1), 2);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        assert_eq!((app.focus, app.scroll_of(0)), (0, 0));
    }

    #[test]
    fn history_spans_the_longest_history_panel() {
        let mut app = app();
        assert_eq!(app.history_span_s(), 86_400);
        app.layout = "missing".into();
        assert_eq!(app.history_span_s(), 86_400, "falls back to tui");
    }
}
