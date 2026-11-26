//! Cores in use over time, stacked by user, with the queue's depth and a time axis below. It doubles as
//! a timeline: the wheel pans it, shift or ctrl with the wheel, or a pinch, zooms it, a drag selects a
//! stretch to zoom to, and a click replays the cluster from that moment. A line marks the moment under
//! the pointer, and another the moment a replay has reached.

use std::collections::HashMap;

use glurmo_model::layout::Span;
use glurmo_model::units::thousands;
use glurmo_model::usage::{Column, Usage};
use glurmo_model::{Login, UnixTime};
use leptos::prelude::*;
use web_sys::CanvasRenderingContext2d;

use crate::paint::{Rgb, Surface, clock, painted};
use crate::replay::{self, SPEEDS};
use crate::state::{State, Timeline};

/// Width of the axis labels on the left, and of one column of the graph. Columns are drawn a pixel wider,
/// so the next one paints over the overlap and fractional device pixels leave no seams.
const LABELS: f64 = 52.0;
const COLUMN: f64 = 3.0;
/// Height of the queue strip and of the time axis under the graph.
const QUEUE: f64 = 22.0;
const AXIS: f64 = 18.0;
/// Seconds between time labels. The axis takes the shortest that keeps labels this far apart.
const LABEL_EVERY_S: [i64; 14] = [
    300, 600, 900, 1_800, 3_600, 7_200, 10_800, 21_600, 43_200, 86_400, 172_800, 604_800,
    1_209_600, 2_592_000,
];
const LABEL_GAP: f64 = 90.0;
/// The shortest and longest spans the timeline zooms to: an hour, and 90 days.
const SPANS_S: (i64, i64) = (3_600, 90 * 86_400);
/// How far the pointer moves before a press is a drag rather than a click.
const DRAG_PX: f64 = 4.0;
/// How much one pixel of wheel zooms.
const ZOOM_PER_PX: f64 = 0.002;
/// Closer than this to the right edge, the cursor's time goes left of its line.
const FLIP_PX: f64 = 170.0;
/// A span longer than this dates the cursor's time as well.
const DAY_S: i64 = 86_400;
/// A click this close to now goes back to live rather than replaying.
const NEAR_NOW_S: f64 = 60.0;

/// The history panel, showing `span` unless the timeline picks another.
#[component]
pub fn History(span: Span) -> impl IntoView {
    let state = expect_context::<State>();
    let span_s = move || {
        state
            .timeline
            .with(|timeline| timeline.span_s)
            .unwrap_or(span.seconds())
    };
    // Every node's cores, up or not, so the scale holds still as nodes fail and between live and replay.
    let installed = Memo::new(move |_| {
        state.cluster.with(|cluster| {
            cluster
                .nodes
                .iter()
                .map(|node| u64::from(node.cores))
                .sum::<u64>()
        })
    });
    let draw = move |context: &CanvasRenderingContext2d, width: f64, height: f64| {
        let Some(theme) = state.theme.get() else {
            return;
        };
        let mono = format!("500 11px \"{}\", monospace", theme.fonts.mono);
        context.set_font(&mono);
        let muted = Rgb::of(&theme.ui.muted);
        state.usage.with(|usage| {
            let usage = match usage {
                Ok(usage) if !usage.steps.is_empty() => usage,
                Ok(_) => return note(context, "no history here", muted),
                Err(error) => return note(context, &format!("no history: {error}"), muted),
            };
            let graph_w = width - LABELS;
            let graph_h = height - QUEUE - AXIS;
            if graph_w < COLUMN || graph_h < 10.0 {
                return;
            }
            let span_s = span_s();
            let from = chart_end(state, usage) - span_s;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a panel is far narrower than u16::MAX columns"
            )]
            let count = (graph_w / COLUMN).floor() as u16;
            let columns = usage.columns(from, span_s, count);
            let total = |column: &Column| column.cores.iter().map(|(_, cores)| cores).sum::<u64>();
            let peak = columns.iter().map(total).max().unwrap_or(0);
            let scale = installed.get().max(peak).max(1);
            let mut colors: HashMap<&str, String> = HashMap::new();
            state.coloring.with(|coloring| {
                let Some(coloring) = coloring else {
                    return;
                };
                for (i, column) in columns.iter().enumerate() {
                    #[expect(clippy::cast_precision_loss, reason = "a small count")]
                    let x = LABELS + i as f64 * COLUMN;
                    let mut y = graph_h;
                    for &(login, cores) in column.cores.iter().filter(|(_, cores)| *cores > 0) {
                        #[expect(clippy::cast_precision_loss, reason = "core counts fit in an f64")]
                        let h = graph_h * cores as f64 / scale as f64;
                        let color = colors.entry(login).or_insert_with(|| {
                            let color = coloring.user(&Login(login.to_owned()));
                            painted(&theme, coloring, color).css()
                        });
                        context.set_fill_style_str(color);
                        context.fill_rect(x, y - h, COLUMN + 1.0, h);
                        y -= h;
                    }
                }
            });
            context.set_fill_style_str(&muted.css());
            context.set_text_align("right");
            let _ = context.fill_text(&thousands(scale), LABELS - 6.0, 10.0);
            let _ = context.fill_text("0", LABELS - 6.0, graph_h);
            let _ = context.fill_text("queue", LABELS - 6.0, graph_h + QUEUE - 6.0);
            context.set_text_align("left");
            queue(context, &columns, graph_h, Rgb::of(&theme.state.drain));
            axis(context, from, span_s, graph_w, height, muted);
        });
    };
    view! {
        <section class="panel history">
            <h2>
                "cores in use"
                <small>{move || describe(state)}</small>
                <SpanPicker span_s=Signal::derive(span_s)/>
                <Controls/>
            </h2>
            <Scrub span_s=Signal::derive(span_s)>
                <Surface draw=draw/>
            </Scrub>
        </section>
    }
}

/// The timeline's input around `children`: the wheel pans it, shift or ctrl with the wheel, or a pinch,
/// zooms it around the pointer, a drag selects a stretch to zoom to, and a click replays from the moment
/// under the pointer, or goes back to live near now.
#[component]
fn Scrub(span_s: Signal<i64>, children: Children) -> impl IntoView {
    let state = expect_context::<State>();
    let press = StoredValue::new(None::<Drag>);
    let hover = RwSignal::new(None::<Hover>);
    let selection = RwSignal::new(None::<(f64, f64)>);
    let pressed = move |event: web_sys::PointerEvent| {
        if state.editing.get_untracked() || event.button() != 0 {
            return;
        }
        let Some(spot) = Spot::of(&event, span_s.get_untracked()) else {
            return;
        };
        if let Some(target) = event.current_target() {
            let element: &web_sys::Element = wasm_bindgen::JsCast::unchecked_ref(&target);
            let _ = element.set_pointer_capture(event.pointer_id());
        }
        press.set_value(Some(Drag {
            x: spot.x.max(LABELS),
            at: moment(state, &spot),
            moved: false,
        }));
    };
    let moved = move |event: web_sys::PointerEvent| {
        let spot = Spot::of(&event, span_s.get_untracked());
        hover.set(
            spot.as_ref()
                .filter(|spot| spot.x >= LABELS)
                .map(|spot| Hover {
                    x: spot.x,
                    at: moment(state, spot),
                    flip: spot.room < FLIP_PX,
                }),
        );
        let (Some(mut held), Some(spot)) = (press.get_value(), spot) else {
            return;
        };
        held.moved |= (spot.x - held.x).abs() > DRAG_PX;
        if held.moved {
            let x = spot.x.clamp(LABELS, spot.x + spot.room);
            selection.set(Some((held.x.min(x), (held.x - x).abs())));
        }
        press.set_value(Some(held));
    };
    let released = move |event: web_sys::PointerEvent| {
        let Some(held) = press.get_value() else {
            return;
        };
        press.set_value(None);
        selection.set(None);
        let Some(spot) = Spot::of(&event, span_s.get_untracked()) else {
            return;
        };
        if held.moved {
            let (span, end) = zoom_to(held.at, moment(state, &spot));
            state.timeline.set(Timeline {
                span_s: Some(span),
                end: follow_now(end),
            });
            return;
        }
        if spot.x < LABELS {
            return;
        }
        #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
        let at = view_end(state) as f64 - spot.before_end_s;
        #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
        let now = crate::paint::now().0 as f64;
        if at >= now - NEAR_NOW_S {
            replay::live(state);
        } else {
            replay::seek(state, at);
        }
    };
    // Wheel input gathers here and moves the timeline once per frame, since each move redraws the chart.
    let wheeling = StoredValue::new(None::<Timeline>);
    let wheeled = move |event: web_sys::WheelEvent| {
        if state.editing.get_untracked() {
            return;
        }
        let pending = wheeling.get_value();
        let base = pending.unwrap_or_else(|| state.timeline.get_untracked());
        let span = base.span_s.unwrap_or_else(|| span_s.get_untracked());
        let Some(spot) = Spot::of(&event, span) else {
            return;
        };
        event.prevent_default();
        let end = base.end.map_or_else(|| crate::paint::now().0, |end| end.0);
        let next = if event.ctrl_key() || event.shift_key() {
            // Browsers turn a shifted wheel into a horizontal one.
            let wheel = if event.delta_y() == 0.0 {
                event.delta_x()
            } else {
                event.delta_y()
            };
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "seconds fit in an f64 and back"
            )]
            let zoomed =
                ((span as f64 * (wheel * ZOOM_PER_PX).exp()) as i64).clamp(SPANS_S.0, SPANS_S.1);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "seconds fit in an f64 and back"
            )]
            let anchored = end - (spot.before_end_s * (1.0 - zoomed as f64 / span as f64)) as i64;
            Timeline {
                span_s: Some(zoomed),
                end: base.end.and_then(|_| follow_now(anchored)),
            }
        } else {
            let wheel = if event.delta_x().abs() > event.delta_y().abs() {
                event.delta_x()
            } else {
                event.delta_y()
            };
            #[expect(clippy::cast_possible_truncation, reason = "seconds fit in an i64")]
            let shift = (wheel * spot.per_px) as i64;
            Timeline {
                end: follow_now(end + shift),
                ..base
            }
        };
        wheeling.set_value(Some(next));
        if pending.is_none() {
            request_animation_frame(move || {
                if let Some(Some(next)) = wheeling.try_get_value() {
                    wheeling.set_value(None);
                    state.timeline.try_set(next);
                }
            });
        }
    };
    view! {
        <div
            class="timeline"
            on:pointerdown=pressed
            on:pointermove=moved
            on:pointerup=released
            on:pointercancel=move |_| {
                press.set_value(None);
                selection.set(None);
            }
            on:pointerleave=move |_| hover.set(None)
            on:wheel=wheeled
        >
            {children()}
            {move || {
                selection.get().map(|(left, width)| {
                    view! {
                        <div
                            class="selection"
                            style:left=format!("{left}px")
                            style:width=format!("{width}px")
                        ></div>
                    }
                })
            }}
            {move || {
                let at = state.replay.get()?.at;
                let span_s = span_s.get();
                let end = state.usage.with(|usage| Some(chart_end(state, usage.as_ref().ok()?)))?;
                #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
                let share = (at - (end - span_s) as f64) / span_s as f64;
                (0.0..=1.0).contains(&share).then(|| {
                    let left = format!("calc({LABELS}px + {share} * (100% - {LABELS}px))");
                    view! { <div class="playhead" style:left=left></div> }
                })
            }}
            {move || {
                hover.get().map(|Hover { x, at, flip }| {
                    let dated = span_s.get() > DAY_S;
                    view! {
                        <div class="cursor" class:flip=flip style:left=format!("{x}px")>
                            <span>{clock(UnixTime(at), dated)}</span>
                        </div>
                    }
                })
            }}
        </div>
    }
}

/// Where the pointer rests over the graph, and the moment under it.
#[derive(Clone, Copy)]
struct Hover {
    x: f64,
    at: i64,
    /// Whether the time goes left of the line, near the right edge.
    flip: bool,
}

/// A press on the timeline: where it started, and the moment there.
#[derive(Clone, Copy)]
struct Drag {
    x: f64,
    at: i64,
    moved: bool,
}

/// The moment under the pointer, within the graph.
fn moment(state: State, spot: &Spot) -> i64 {
    #[expect(clippy::cast_possible_truncation, reason = "seconds fit in an i64")]
    let before_end = spot.before_end_s.max(0.0) as i64;
    view_end(state) - before_end
}

/// The span and end that show moments `a` and `b` edge to edge. A stretch shorter than the shortest
/// span widens around its middle, and one longer than the longest keeps its end.
fn zoom_to(a: i64, b: i64) -> (i64, i64) {
    let (from, to) = (a.min(b), a.max(b));
    let span = (to - from).clamp(SPANS_S.0, SPANS_S.1);
    if to - from < span {
        (span, from + (to - from) / 2 + span / 2)
    } else {
        (span, to)
    }
}

/// Where a pointer is on the timeline.
struct Spot {
    /// From the timeline's left edge, in CSS pixels.
    x: f64,
    /// Seconds of history per pixel of graph.
    per_px: f64,
    /// How long before the timeline's end the pointer's moment is.
    before_end_s: f64,
    /// CSS pixels from the pointer to the timeline's right edge.
    room: f64,
}

impl Spot {
    fn of(event: &web_sys::MouseEvent, span_s: i64) -> Option<Self> {
        let target = event.current_target()?;
        let element: &web_sys::Element = wasm_bindgen::JsCast::unchecked_ref(&target);
        let frame = element.get_bounding_client_rect();
        let graph_w = frame.width() - LABELS;
        if graph_w < COLUMN {
            return None;
        }
        let x = f64::from(event.client_x()) - frame.left();
        #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
        let per_px = span_s as f64 / graph_w;
        Some(Self {
            x,
            per_px,
            before_end_s: (frame.width() - x.max(LABELS)) * per_px,
            room: frame.width() - x,
        })
    }
}

/// Where the chart ends: where the timeline was panned to, else where `usage` was read up to.
fn chart_end(state: State, usage: &Usage) -> i64 {
    state
        .timeline
        .with(|timeline| timeline.end)
        .map_or(usage.to.0, |end| end.0)
}

/// Where the timeline ends now, in seconds since the epoch.
fn view_end(state: State) -> i64 {
    state
        .timeline
        .with_untracked(|timeline| timeline.end)
        .map_or_else(|| crate::paint::now().0, |end| end.0)
}

/// An end at `end`, or `None` to follow now once it reaches now.
fn follow_now(end: i64) -> Option<UnixTime> {
    (end < crate::paint::now().0).then_some(UnixTime(end))
}

/// Where the timeline ends, when it does not follow now. The span picker shows the span.
fn describe(state: State) -> String {
    let end = state.timeline.with(|timeline| timeline.end);
    let to = end.map_or_else(String::new, |end| format!(" · to {}", clock(end, true)));
    format!("stacked by user · queue in tasks{to}")
}

/// A span in its largest whole unit, such as `6h` or `3d`.
fn length(span_s: i64) -> String {
    if span_s % 86_400 == 0 {
        format!("{}d", span_s / 86_400)
    } else {
        format!("{}h", span_s / 3_600)
    }
}

/// A span typed as a count of hours or days, such as `6h` or `3 d`.
pub fn parse_span(text: &str) -> Option<i64> {
    let text = text.trim();
    let (count, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let unit = match unit.trim() {
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    Some(count.parse::<i64>().ok()? * unit)
}

/// The timeline's span as text to edit, and a button back to now once the timeline is panned away.
#[component]
fn SpanPicker(span_s: Signal<i64>) -> impl IntoView {
    let state = expect_context::<State>();
    let typed = move |event: web_sys::Event| {
        if let Some(span) = parse_span(&event_target_value(&event)) {
            state.timeline.update(|timeline| {
                timeline.span_s = Some(span.clamp(SPANS_S.0, SPANS_S.1));
            });
        }
        // Text that names no span, or the span already shown, leaves the signal unchanged, so the
        // binding would not rewrite the field.
        let field: web_sys::HtmlInputElement = event_target(&event);
        field.set_value(&length(span_s.get_untracked()));
    };
    view! {
        <span class="controls">
            <input
                class="span"
                type="text"
                spellcheck="false"
                title="span, such as 6h or 3d"
                prop:value=move || length(span_s.get())
                on:change=typed
            />
            {move || {
                state
                    .timeline
                    .with(|timeline| timeline.end.is_some())
                    .then(|| {
                        view! {
                            <button on:click=move |_| {
                                state.timeline.update(|timeline| timeline.end = None);
                            }>"now"</button>
                        }
                    })
            }}
        </span>
    }
}

/// Pause, the speeds and back to live, while a replay runs.
#[component]
fn Controls() -> impl IntoView {
    let state = expect_context::<State>();
    let speed = Memo::new(move |_| state.replay.get().map(|playhead| playhead.speed));
    move || {
        speed.get().map(|speed| {
            let speeds = SPEEDS
                .into_iter()
                .map(|choice| {
                    view! {
                        <button
                            class:on=speed == Some(choice)
                            on:click=move |_| replay::play(state, Some(choice))
                        >
                            {format!("{choice}×")}
                        </button>
                    }
                })
                .collect_view();
            view! {
                <span class="controls replay">
                    <button class:on=speed.is_none() on:click=move |_| replay::play(state, None)>
                        "pause"
                    </button>
                    {speeds}
                    <button on:click=move |_| replay::live(state)>"live"</button>
                </span>
            }
        })
    }
}

fn note(context: &CanvasRenderingContext2d, text: &str, color: Rgb) {
    context.set_fill_style_str(&color.css());
    let _ = context.fill_text(text, LABELS, 16.0);
}

/// Each column's tasks waiting, as bars up from the strip's floor, scaled to the deepest.
fn queue(context: &CanvasRenderingContext2d, columns: &[Column], top: f64, color: Rgb) {
    let deepest = columns
        .iter()
        .map(|column| column.waiting)
        .max()
        .unwrap_or(0)
        .max(1);
    context.set_fill_style_str(&color.css());
    let floor = top + QUEUE - 4.0;
    for (i, column) in columns.iter().enumerate() {
        if column.waiting == 0 {
            continue;
        }
        #[expect(clippy::cast_precision_loss, reason = "counts fit in an f64")]
        let (x, h) = (
            LABELS + i as f64 * COLUMN,
            (QUEUE - 8.0) * column.waiting as f64 / deepest as f64,
        );
        context.fill_rect(x, floor - h.max(1.0), COLUMN + 1.0, h.max(1.0));
    }
}

/// Time labels on whole steps in the browser's time zone, as often as they fit, and only where they
/// fit whole. Steps under a day print the time, and longer ones the date.
fn axis(
    context: &CanvasRenderingContext2d,
    from: i64,
    span_s: i64,
    width: f64,
    height: f64,
    color: Rgb,
) {
    #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
    let per_second = width / span_s as f64;
    let every = LABEL_EVERY_S
        .into_iter()
        .find(|every| {
            #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
            let every = *every as f64;
            every * per_second >= LABEL_GAP
        })
        .unwrap_or(LABEL_EVERY_S[LABEL_EVERY_S.len() - 1]);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "seconds fit in an f64, and a time zone's offset in an i64"
    )]
    let offset_s =
        -(js_sys::Date::new(&(from as f64 * 1_000.0).into()).get_timezone_offset() * 60.0) as i64;
    context.set_fill_style_str(&color.css());
    let mut at = (from + offset_s).div_euclid(every) * every + every - offset_s;
    while at < from + span_s {
        let text = if every >= 86_400 {
            clock(UnixTime(at), true)
                .split(' ')
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            clock(UnixTime(at), false)[..5].to_owned()
        };
        #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
        let x = LABELS + (at - from) as f64 * per_second;
        let text_w = context
            .measure_text(&text)
            .map_or(0.0, |metrics| metrics.width());
        if x + text_w <= LABELS + width {
            let _ = context.fill_text(&text, x, height - 4.0);
        }
        at += every;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_back_as_written() {
        assert_eq!(parse_span("6h"), Some(21_600));
        assert_eq!(parse_span(" 3 d "), Some(259_200));
        assert_eq!(parse_span("90d").map(length).as_deref(), Some("90d"));
        assert_eq!(parse_span("36h").map(length).as_deref(), Some("36h"));
        assert_eq!(parse_span("48h").map(length).as_deref(), Some("2d"));
    }

    #[test]
    fn a_selection_zooms_to_its_edges() {
        assert_eq!(zoom_to(10_000, 30_000), (20_000, 30_000));
        assert_eq!(
            zoom_to(30_000, 10_000),
            (20_000, 30_000),
            "a drag to the left"
        );
    }

    #[test]
    fn a_short_selection_widens_around_its_middle() {
        assert_eq!(zoom_to(10_000, 10_600), (3_600, 10_300 + 1_800));
        let (span, end) = zoom_to(0, 100 * 86_400);
        assert_eq!(
            (span, end),
            (90 * 86_400, 100 * 86_400),
            "a long one keeps its end"
        );
    }

    #[test]
    fn spans_need_a_count_and_a_unit() {
        for text in ["", "6", "h", "6m", "6 hours", "-6h"] {
            assert_eq!(parse_span(text), None, "{text:?}");
        }
    }
}
