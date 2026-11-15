//! Cores in use over time, stacked by user, with the queue's depth and a time axis below. It doubles as
//! a timeline: the wheel or a drag pans it, ctrl and the wheel or a pinch zooms it, and a click replays
//! the cluster from that moment.

use glurmo_model::layout::Span;
use glurmo_model::units::thousands;
use glurmo_model::usage::Column;
use glurmo_model::{Login, UnixTime, summary};
use leptos::prelude::*;
use web_sys::CanvasRenderingContext2d;

use crate::paint::{Rgb, Surface, clock, user_color};
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
    let draw = move |context: &CanvasRenderingContext2d, width: f64, height: f64| {
        let Some(theme) = state.theme.get() else {
            return;
        };
        let mono = format!("500 11px \"{}\", monospace", theme.fonts.mono);
        context.set_font(&mono);
        let muted = Rgb::of(&theme.ui.muted);
        let usage = state.usage.get();
        let usage = match &usage {
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
        let end = state
            .timeline
            .with(|timeline| timeline.end)
            .map_or(usage.to.0, |end| end.0);
        let from = end - span_s;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a panel is far narrower than u16::MAX columns"
        )]
        let count = (graph_w / COLUMN).floor() as u16;
        let columns = usage.columns(from, span_s, count);
        let capacity = state.cluster.with(|cluster| {
            state
                .occupancy
                .with(|occupancy| summary::cores(cluster, occupancy).total)
        });
        let total = |column: &Column| column.cores.iter().map(|(_, cores)| cores).sum::<u64>();
        let peak = columns.iter().map(total).max().unwrap_or(0);
        let scale = capacity.max(peak).max(1);
        state.cluster.with(|cluster| {
            for (i, column) in columns.iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "a small count")]
                let x = LABELS + i as f64 * COLUMN;
                let mut y = graph_h;
                for (login, cores) in &column.cores {
                    #[expect(clippy::cast_precision_loss, reason = "core counts fit in an f64")]
                    let h = graph_h * *cores as f64 / scale as f64;
                    let color = user_color(&theme, cluster, &Login((*login).to_owned()));
                    context.set_fill_style_str(&color.css());
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
        if let Some(playhead) = state.replay.get() {
            #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
            let x = LABELS + (playhead.at - from as f64) / span_s as f64 * graph_w;
            if (LABELS..=width).contains(&x) {
                context.set_fill_style_str(&Rgb::of(&theme.ui.text).css());
                context.fill_rect(x - 1.0, 0.0, 2.0, graph_h + QUEUE);
            }
        }
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

/// The timeline's input around `children`: the wheel or a drag pans it, ctrl and the wheel or a pinch
/// zooms it around the pointer, and a click replays from the moment under the pointer, or goes back to
/// live near now.
#[component]
fn Scrub(span_s: Signal<i64>, children: Children) -> impl IntoView {
    let state = expect_context::<State>();
    let press = StoredValue::new(None::<Drag>);
    let pressed = move |event: web_sys::PointerEvent| {
        if state.editing() || event.button() != 0 {
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
            x: spot.x,
            end: view_end(state),
            moved: false,
        }));
    };
    let moved = move |event: web_sys::PointerEvent| {
        let (Some(mut held), Some(spot)) =
            (press.get_value(), Spot::of(&event, span_s.get_untracked()))
        else {
            return;
        };
        let dx = spot.x - held.x;
        held.moved |= dx.abs() > DRAG_PX;
        if held.moved {
            #[expect(clippy::cast_possible_truncation, reason = "seconds fit in an i64")]
            let shift = (dx * spot.per_px) as i64;
            state
                .timeline
                .update(|timeline| timeline.end = follow_now(held.end - shift));
        }
        press.set_value(Some(held));
    };
    let released = move |event: web_sys::PointerEvent| {
        let Some(held) = press.get_value() else {
            return;
        };
        press.set_value(None);
        let Some(spot) = Spot::of(&event, span_s.get_untracked()).filter(|spot| spot.x >= LABELS)
        else {
            return;
        };
        if held.moved {
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
    let wheeled = move |event: web_sys::WheelEvent| {
        if state.editing() {
            return;
        }
        let Some(spot) = Spot::of(&event, span_s.get_untracked()) else {
            return;
        };
        event.prevent_default();
        let end = view_end(state);
        if event.ctrl_key() {
            let span = span_s.get_untracked();
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "seconds fit in an f64 and back"
            )]
            let zoomed = ((span as f64 * (event.delta_y() * ZOOM_PER_PX).exp()) as i64)
                .clamp(SPANS_S.0, SPANS_S.1);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                reason = "seconds fit in an f64 and back"
            )]
            let anchored = end - (spot.before_end_s * (1.0 - zoomed as f64 / span as f64)) as i64;
            let follows = state
                .timeline
                .with_untracked(|timeline| timeline.end.is_none());
            state.timeline.set(Timeline {
                span_s: Some(zoomed),
                end: if follows { None } else { follow_now(anchored) },
            });
        } else {
            let wheel = if event.delta_x().abs() > event.delta_y().abs() {
                event.delta_x()
            } else {
                event.delta_y()
            };
            #[expect(clippy::cast_possible_truncation, reason = "seconds fit in an i64")]
            let shift = (wheel * spot.per_px) as i64;
            state
                .timeline
                .update(|timeline| timeline.end = follow_now(end + shift));
        }
    };
    view! {
        <div
            class="timeline"
            on:pointerdown=pressed
            on:pointermove=moved
            on:pointerup=released
            on:pointercancel=move |_| press.set_value(None)
            on:wheel=wheeled
        >
            {children()}
        </div>
    }
}

/// A press on the timeline: where it started, and where the timeline ended then.
#[derive(Clone, Copy)]
struct Drag {
    x: f64,
    end: i64,
    moved: bool,
}

/// Where a pointer is on the timeline.
struct Spot {
    /// From the timeline's left edge, in CSS pixels.
    x: f64,
    /// Seconds of history per pixel of graph.
    per_px: f64,
    /// How long before the timeline's end the pointer's moment is.
    before_end_s: f64,
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
        })
    }
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
fn parse_span(text: &str) -> Option<i64> {
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
    fn spans_need_a_count_and_a_unit() {
        for text in ["", "6", "h", "6m", "6 hours", "-6h"] {
            assert_eq!(parse_span(text), None, "{text:?}");
        }
    }
}
