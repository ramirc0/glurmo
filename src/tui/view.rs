//! Lays a layout's panels out over the terminal and gives them what they draw from.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction as Axis, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};

use super::app::App;
use super::theme_colors::{ThemeColors, rgb};
use super::{groups, header, history, jobs, mine, nodes, partitions, queue, ticker, tiles, users};
use glurmo_model::jobs::Words;
use glurmo_model::layout::{Direction, Pane, Size};
use glurmo_model::palette::Coloring;
use glurmo_model::summary::{self, Tally};
use glurmo_model::theme;
use glurmo_model::{Attempt, Cluster, Login, Occupancies, Owner, UnixTime};

/// What every panel draws from, worked out once per frame.
pub struct Scene<'a> {
    /// The state.
    pub app: &'a App,
    /// The theme's colors.
    pub tones: ThemeColors,
    /// Who takes which color under the palette.
    pub coloring: Coloring,
    /// Who holds each core and GPU.
    pub occupancy: Occupancies,
    /// The time of the last delta, which ages count from.
    pub now: UnixTime,
    /// The filter's words.
    pub filter: Words,
}

/// Whether a panel takes scroll keys, and how far it is scrolled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Focus {
    /// Whether it takes scroll keys.
    pub focused: bool,
    /// Lines scrolled.
    pub scroll: u16,
}

impl<'a> Scene<'a> {
    /// The scene for one frame of `app`.
    pub fn new(app: &'a App) -> Self {
        let occupancy = app.cluster.occupancy();
        Self {
            app,
            tones: ThemeColors::of(app.theme()),
            coloring: app.palette().coloring(&app.cluster, &occupancy),
            occupancy,
            now: app.cluster.at.unwrap_or_default(),
            filter: Words::of(app.filter().unwrap_or_default()),
        }
    }

    /// The cluster.
    pub fn cluster(&self) -> &Cluster {
        &self.app.cluster
    }

    /// The attempt that holds a cell, if it is visible.
    pub fn attempt(&self, owner: Owner) -> Option<&Attempt> {
        match owner {
            Owner::Attempt(key) => self.cluster().running.get(&key).map(|r| &r.attempt),
            Owner::Hidden => None,
        }
    }

    /// The color of `login` under the palette.
    pub fn color(&self, login: &Login) -> Color {
        self.painted(self.coloring.user(login))
    }

    /// A palette color for the terminal. `None` is the rest color, which is the palette's own or a
    /// quiet one from the theme.
    pub fn painted(&self, color: Option<&theme::Color>) -> Color {
        color
            .or(self.coloring.rest())
            .map_or_else(|| self.tones.fade(self.tones.muted, 0.55), rgb)
    }

    /// How to show `login`: the full name when the server sends one.
    pub fn name<'b>(&'b self, login: &'b Login) -> &'b str {
        self.cluster()
            .users
            .get(login)
            .and_then(|user| user.name.as_deref())
            .unwrap_or(&login.0)
    }

    /// Whether every word of the filter appears in one of `texts`.
    pub fn shows(&self, texts: &[&str]) -> bool {
        self.filter.found_in(texts)
    }

    /// Whether the filter lets an attempt through, as it does in the job list.
    pub fn shows_attempt(&self, attempt: &Attempt) -> bool {
        self.filter.found_in_attempt(self.cluster(), attempt)
    }

    /// The color of a core or GPU: its holder's, faded when the filter hides the holder, or `empty`.
    pub fn cell(&self, owner: Option<Owner>, empty: Color) -> Color {
        match owner {
            None => empty,
            Some(Owner::Hidden) => self.tones.muted,
            Some(owner) => match self.attempt(owner) {
                Some(attempt) => {
                    let color = self.painted(self.coloring.core(attempt, self.now));
                    if self.shows_attempt(attempt) {
                        color
                    } else {
                        self.tones.fade(color, 0.3)
                    }
                }
                None => self.tones.muted,
            },
        }
    }

    /// A panel's frame, with `title` in the top border and `note` in the bottom one.
    pub fn block(&self, title: &str, note: &str, focus: Focus) -> Block<'static> {
        let border = if focus.focused {
            self.tones.subtle
        } else {
            self.tones.fade(self.tones.muted, 0.5)
        };
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .title(Line::from(Span::styled(
                format!(" {title} "),
                Style::new()
                    .fg(self.tones.text)
                    .add_modifier(Modifier::BOLD),
            )));
        if !note.is_empty() {
            block = block.title_bottom(
                Line::from(Span::styled(format!(" {note} "), self.tones.muted)).right_aligned(),
            );
        }
        block
    }

    /// Cores on nodes that can run jobs, and how many of them jobs hold.
    pub fn cores(&self) -> Tally {
        summary::cores(self.cluster(), &self.occupancy)
    }

    /// GPUs on nodes that can run jobs, and how many of them jobs hold.
    pub fn gpus(&self) -> Tally {
        summary::gpus(self.cluster(), &self.occupancy)
    }
}

/// The panels of a layout, in the order focus visits them, splits left out.
pub fn panels(root: &Pane) -> Vec<&Pane> {
    match root {
        Pane::Split(_, children) => children
            .iter()
            .flat_map(|(_, child)| panels(child))
            .collect(),
        pane => vec![pane],
    }
}

/// The panels that take focus, in the order `tab` visits them.
fn focusable(root: &Pane) -> Vec<&Pane> {
    panels(root)
        .into_iter()
        .filter(|pane| !matches!(pane, Pane::Header | Pane::Keys | Pane::History { .. }))
        .collect()
}

/// Draws a frame of `app`: the layout, or the focused panel alone when it fills the screen. Returns how
/// far each scrolling panel could scroll. A panel hidden behind a filled one keeps its last limit.
pub fn draw(frame: &mut Frame, app: &App) -> Vec<u16> {
    let scene = Scene::new(app);
    let area = frame.area();
    let buf = frame.buffer_mut();
    buf.set_style(area, Style::new().bg(scene.tones.base).fg(scene.tones.text));
    let root = &app.layout().root;
    let filled = app
        .filled
        .then(|| focusable(root).get(app.focus).copied())
        .flatten();
    let mut limits = Vec::new();
    match filled {
        Some(pane) => {
            limits.extend(app.limits.iter().take(app.focus));
            walk(&scene, pane, area, buf, &mut limits);
            limits.extend(app.limits.iter().skip(app.focus + 1));
        }
        None => walk(&scene, root, area, buf, &mut limits),
    }
    if app.filter.editing {
        prompt(&scene, area, buf);
    }
    if app.help {
        help(&scene, area, buf);
    }
    limits
}

fn walk(scene: &Scene, pane: &Pane, area: Rect, buf: &mut Buffer, limits: &mut Vec<u16>) {
    let index = limits.len();
    let focus = Focus {
        focused: index == scene.app.focus,
        scroll: scene.app.scroll_of(index),
    };
    let limit = match pane {
        Pane::Split(direction, children) => {
            let axis = match direction {
                Direction::Vertical => Axis::Vertical,
                Direction::Horizontal => Axis::Horizontal,
            };
            let constraints = children.iter().map(|(size, _)| constraint(*size));
            let areas = ratatui::layout::Layout::new(axis, constraints).split(area);
            for ((_, child), area) in children.iter().zip(areas.iter()) {
                walk(scene, child, *area, buf, limits);
            }
            return;
        }
        Pane::Header => return header::header(scene, area, buf),
        Pane::Keys => return header::keys(scene, area, buf),
        Pane::History { span } => return history::draw(scene, *span, area, buf),
        Pane::Tiles { group } => tiles::draw(scene, *group, area, buf, focus),
        Pane::Users => users::draw(scene, area, buf, focus),
        Pane::Groups => groups::draw(scene, area, buf, focus),
        Pane::Queue { by } => queue::draw(scene, *by, area, buf, focus),
        Pane::Partitions => partitions::draw(scene, area, buf, focus),
        Pane::Nodes => nodes::draw(scene, area, buf, focus),
        Pane::Mine => mine::draw(scene, area, buf, focus),
        Pane::Jobs => jobs::draw(scene, area, buf, focus),
        Pane::Ticker => ticker::draw(scene, area, buf, focus),
    };
    limits.push(limit);
}

fn constraint(size: Size) -> Constraint {
    match size {
        Size::Length(n) => Constraint::Length(n),
        Size::Percentage(n) => Constraint::Percentage(n),
        Size::Fill(n) => Constraint::Fill(n),
        Size::Min(n) => Constraint::Min(n),
    }
}

/// Draws `lines` under an optional `head` row inside `block`, scrolled by `focus`.
/// Returns how far the lines could scroll.
pub fn scrolled(
    block: Block,
    head: Option<Line>,
    lines: Vec<Line>,
    area: Rect,
    buf: &mut Buffer,
    focus: Focus,
) -> u16 {
    let inner = block.inner(area);
    block.render(area, buf);
    let mut body = inner;
    if let Some(head) = head
        && body.height > 0
    {
        buf.set_line(body.x, body.y, &head, body.width);
        body.y += 1;
        body.height -= 1;
    }
    let count = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let limit = count.saturating_sub(body.height);
    Paragraph::new(lines)
        .scroll((focus.scroll.min(limit), 0))
        .render(body, buf);
    limit
}

/// The filter being typed, over the bottom line.
fn prompt(scene: &Scene, area: Rect, buf: &mut Buffer) {
    let tones = &scene.tones;
    let line = Line::from(vec![
        Span::styled(" filter ", Style::new().fg(tones.base).bg(tones.drain)),
        Span::styled(format!(" {}▏", scene.app.filter.text), tones.text),
        Span::styled("   enter keeps it, esc clears it", tones.muted),
    ]);
    let bottom = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.width,
        1.min(area.height),
    );
    buf.set_style(bottom, Style::new().bg(tones.base));
    buf.set_line(bottom.x, bottom.y, &line, bottom.width);
}

/// What each key does, over the middle of the screen.
fn help(scene: &Scene, area: Rect, buf: &mut Buffer) {
    let keys = [
        ("q", "quit"),
        ("tab", "focus the next panel"),
        ("↑ ↓ j k", "scroll the focused panel"),
        ("/", "filter by user, partition or job name"),
        ("m", "show only your jobs, or everyone again"),
        ("f", "fill the screen with the focused panel"),
        ("esc", "clear the filter, then the fill"),
        ("l", "next layout"),
        ("t", "next theme"),
        ("p", "next palette"),
        ("?", "this help"),
    ];
    let tones = &scene.tones;
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:>8}  "), tones.drain),
                Span::styled(*what, tones.text),
            ])
        })
        .collect();
    let width = 52.min(area.width);
    let height = (u16::try_from(lines.len()).unwrap_or(0) + 2).min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    Clear.render(popup, buf);
    buf.set_style(popup, Style::new().bg(tones.base));
    let block = scene.block(
        "keys",
        "any key closes",
        Focus {
            focused: true,
            scroll: 0,
        },
    );
    Paragraph::new(lines).block(block).render(popup, buf);
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use jiff::tz::TimeZone;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crossterm::event::{KeyCode, KeyEvent};

    use std::collections::BTreeSet;

    use super::*;
    use crate::config::{Tui, from_ron};
    use crate::hub::Message;
    use crate::library::Library;
    use crate::tui::feed::{Link, Update};
    use glurmo_model::jobs::{Order, Search, job_groups};
    use glurmo_model::usage::Step;
    use glurmo_model::usage::Usage;
    use glurmo_model::{AttemptKey, GroupName, UnixTime};

    /// The busy quarter hour as a view sees it after following every delta, seen by the user with
    /// the most running jobs, over a made-up day of usage by the four busiest users.
    pub(in crate::tui) async fn busy_app() -> App {
        let hub = crate::fixture::busy_stored_hub().await;
        let mut app = App::new(
            Library::load(Path::new("/nonexistent")),
            &Tui::default(),
            Login(String::new()),
            TimeZone::UTC,
        );
        app.site = Some(hub.snapshot().site);
        for delta in hub.deltas() {
            app.update(Update::Message(Message::Delta(delta)));
        }
        app.update(Update::Link(Link::Live));
        let mut jobs: BTreeMap<&Login, usize> = BTreeMap::new();
        for running in app.cluster.running.values() {
            *jobs.entry(&running.attempt.login).or_default() += 1;
        }
        let mut busiest: Vec<(&Login, usize)> = jobs.into_iter().collect();
        busiest.sort_by_key(|(login, jobs)| (std::cmp::Reverse(*jobs), (*login).clone()));
        let logins: Vec<Login> = busiest
            .iter()
            .take(4)
            .map(|(login, _)| (*login).clone())
            .collect();
        app.me = logins[0].clone();
        let to = app.cluster.at.unwrap();
        let from = UnixTime(to.0 - 86_400);
        let mut steps = Vec::new();
        for i in 0..288_u64 {
            for (rank, login) in (0_u64..).zip(&logins) {
                let cores = (200 + 150 * ((i / 24 + rank) % 4)) / (rank + 1);
                steps.push(Step {
                    at: UnixTime(from.0 / 300 * 300 + i.cast_signed() * 300),
                    key: login.0.clone(),
                    core_s: cores * 300,
                    wait_s: (i % 48) * 3_000 * (rank + 1),
                });
            }
        }
        app.update(Update::Usage(Ok(Usage {
            from,
            to,
            step_s: 300,
            steps,
        })));
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| app.limits = draw(frame, app))
            .unwrap();
        terminal
    }

    #[tokio::test]
    async fn the_tui_layout_at_160_by_50() {
        let mut app = busy_app().await;
        insta::assert_snapshot!(render(&mut app, 160, 50).backend());
    }

    #[tokio::test]
    async fn the_tui_layout_at_100_by_30() {
        let mut app = busy_app().await;
        insta::assert_snapshot!(render(&mut app, 100, 30).backend());
    }

    #[tokio::test]
    async fn every_other_panel_with_a_filter_and_help() {
        let mut app = busy_app().await;
        let layout = from_ron(
            r#"Layout(name: "rest", root: Split(Horizontal, [
                (Fill(1), Split(Vertical, [
                    (Fill(1), Tiles(group: Partition)),
                    (Length(8), Mine),
                ])),
                (Length(64), Split(Vertical, [
                    (Length(12), Partitions),
                    (Length(8), Queue(by: Login)),
                    (Fill(1), Ticker),
                    (Length(1), Keys),
                ])),
            ]))"#,
        )
        .unwrap();
        app.library.layouts.insert("rest".into(), layout);
        app.layout = "rest".into();
        app.filter.text = "4hours".into();
        let mut terminal = render(&mut app, 160, 50);
        insta::assert_snapshot!("rest_filtered", terminal.backend());
        app.help = true;
        terminal
            .draw(|frame| app.limits = draw(frame, &app))
            .unwrap();
        insta::assert_snapshot!("rest_help", terminal.backend());
        assert_eq!(
            app.limits.len(),
            5,
            "tiles, mine, partitions, queue, ticker"
        );
    }

    #[tokio::test]
    async fn the_groups_panel_sums_each_groups_members() {
        let mut app = busy_app().await;
        for (index, user) in app.cluster.users.values_mut().enumerate() {
            let member_of = [(2, "encode"), (3, "mohd")];
            user.groups = member_of
                .iter()
                .filter(|(every, _)| index % every == 0)
                .map(|(_, name)| GroupName((*name).to_owned()))
                .collect();
        }
        let layout = from_ron(r#"Layout(name: "groups", root: Groups)"#).unwrap();
        app.library.layouts.insert("groups".into(), layout);
        app.layout = "groups".into();
        let terminal = render(&mut app, 70, 6);
        insta::assert_snapshot!("groups", terminal.backend());
    }

    #[tokio::test]
    async fn a_palette_colors_the_biggest_holders_and_the_rest_alike() {
        let mut app = busy_app().await;
        let held = summary::holdings(&app.cluster, &app.cluster.occupancy());
        let logins: Vec<Login> = held
            .users
            .iter()
            .map(|(login, _)| (*login).clone())
            .collect();
        assert!(held.users[2].1.cores > 0, "three users hold cores");
        let two =
            from_ron(r##"Palette(name: "two", colors: ["#111111", "#222222"], rest: "#333333")"##)
                .unwrap();
        app.library.palettes.insert("two".into(), two);
        app.palette = Some("two".into());
        let scene = Scene::new(&app);
        let colors: Vec<Color> = logins
            .iter()
            .take(4)
            .map(|login| scene.color(login))
            .collect();
        let grey = |level| Color::Rgb(level, level, level);
        assert_eq!(colors, [grey(0x11), grey(0x22), grey(0x33), grey(0x33)]);
    }

    #[tokio::test]
    async fn p_goes_through_the_palettes_and_back_to_the_themes_own() {
        let mut app = busy_app().await;
        let mut seen = Vec::new();
        for _ in 0..=app.library.palettes.len() {
            app.key(crossterm::event::KeyEvent::from(
                crossterm::event::KeyCode::Char('p'),
            ));
            seen.push(app.palette.clone());
        }
        let names: Vec<Option<String>> = app.library.palettes.keys().cloned().map(Some).collect();
        assert_eq!(seen, [names, vec![None]].concat());
    }

    #[tokio::test]
    async fn a_time_left_palette_shows_its_steps_on_the_cores_frame() {
        let mut app = busy_app().await;
        let layout = from_ron(r#"Layout(name: "tiles", root: Tiles(group: Hardware))"#).unwrap();
        app.library.layouts.insert("tiles".into(), layout);
        app.layout = "tiles".into();
        app.palette = Some("time-left".into());
        let terminal = render(&mut app, 100, 3);
        insta::assert_snapshot!("time_left_legend", terminal.backend());
    }

    #[tokio::test]
    async fn squeezed_panes_and_a_zero_history_span_draw_without_panicking() {
        let mut app = busy_app().await;
        for (name, root) in [
            (
                "squeezed",
                "Split(Vertical, [(Min(10), Users), (Length(1), Header), (Length(1), Keys)])",
            ),
            (
                "flat",
                "Split(Vertical, [(Fill(1), Users), (Length(12), History(span: Hours(0)))])",
            ),
        ] {
            let layout = from_ron(&format!(r#"Layout(name: "{name}", root: {root})"#)).unwrap();
            app.library.layouts.insert(name.into(), layout);
            app.layout = name.into();
            render(&mut app, 100, 10);
            render(&mut app, 100, 30);
        }
    }

    #[tokio::test]
    async fn the_focused_panel_fills_the_screen_until_f_or_esc() {
        let mut app = busy_app().await;
        let layout = render(&mut app, 100, 30).backend().to_string();
        let panels = app.limits.len();
        app.key(KeyEvent::from(KeyCode::Tab));
        app.key(KeyEvent::from(KeyCode::Char('f')));
        insta::assert_snapshot!("filled_users", render(&mut app, 100, 30).backend());
        assert_eq!(app.limits.len(), panels, "hidden panels keep their limits");
        app.key(KeyEvent::from(KeyCode::Tab));
        let next = render(&mut app, 100, 30).backend().to_string();
        assert!(
            next.contains("queue"),
            "tab moves the fill to the next panel"
        );
        app.key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.quit);
        app.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(render(&mut app, 100, 30).backend().to_string(), layout);
    }

    #[tokio::test]
    async fn the_jobs_layout_groups_jobs_and_widens_when_filled() {
        let mut app = busy_app().await;
        app.layout = "jobs".into();
        insta::assert_snapshot!("jobs_layout", render(&mut app, 160, 50).backend());
        app.key(KeyEvent::from(KeyCode::Tab));
        app.key(KeyEvent::from(KeyCode::Char('f')));
        app.filter.text = "4hours".into();
        insta::assert_snapshot!("jobs_filled", render(&mut app, 200, 30).backend());
    }

    #[tokio::test]
    async fn every_panel_reads_the_filter_as_the_job_list_does() {
        let mut app = busy_app().await;
        let running = &app.cluster.running.values().next().unwrap().attempt;
        let pending = app.cluster.queue.values().next().unwrap();
        let filters = [
            format!("{} {}", running.login.0, running.partitions[0].0),
            running.printed_id(),
            running.nodes[0].0.to_uppercase(),
            format!("{} {}", pending.login.0, pending.reason.clone().unwrap()),
        ];
        for filter in filters {
            app.filter.text.clone_from(&filter);
            let scene = Scene::new(&app);
            let search = Search {
                text: &filter,
                ..Search::default()
            };
            let listed: BTreeSet<AttemptKey> = job_groups(&app.cluster, &search, Order::Cpus)
                .iter()
                .flat_map(|group| group.attempts.iter().map(|attempt| attempt.key))
                .collect();
            assert!(!listed.is_empty(), "{filter:?} lists nothing");
            let running = app.cluster.running.values().map(|running| &running.attempt);
            for attempt in running.chain(app.cluster.queue.values()) {
                assert_eq!(
                    scene.shows_attempt(attempt),
                    listed.contains(&attempt.key),
                    "{filter:?} on {}",
                    attempt.printed_id()
                );
            }
        }
        app.filter.text = "Z10 drain".into();
        assert!(
            Scene::new(&app).shows(&["z1014", "draining"]),
            "words may match apart"
        );
    }
}
