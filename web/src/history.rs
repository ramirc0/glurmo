//! Cores in use over time, stacked by user, with the queue's depth and a time axis below.

use leptos::prelude::*;
use rtviz_model::layout::Span;
use rtviz_model::units::thousands;
use rtviz_model::usage::Column;
use rtviz_model::{Login, UnixTime, summary};
use web_sys::CanvasRenderingContext2d;

use crate::paint::{Rgb, Surface, clock, user_color};
use crate::state::State;

/// Width of the axis labels on the left, and of one column of the graph.
const LABELS: f64 = 52.0;
const COLUMN: f64 = 3.0;
/// Height of the queue strip and of the time axis under the graph.
const QUEUE: f64 = 22.0;
const AXIS: f64 = 18.0;
/// Hours between time labels. The axis takes the fewest that keep labels this far apart.
const LABEL_EVERY_H: [i64; 9] = [1, 2, 3, 6, 12, 24, 48, 168, 336];
const LABEL_GAP: f64 = 90.0;

/// The history panel.
#[component]
pub fn History(span: Span) -> impl IntoView {
    let state = expect_context::<State>();
    let label = match span {
        Span::Hours(hours) => format!("{hours} h"),
        Span::Days(days) => format!("{days} d"),
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
            Ok(_) => return note(context, "no history yet", muted),
            Err(error) => return note(context, &format!("no history: {error}"), muted),
        };
        let graph_w = width - LABELS;
        let graph_h = height - QUEUE - AXIS;
        if graph_w < COLUMN || graph_h < 10.0 {
            return;
        }
        let span_s = span.seconds();
        let from = usage.to.0 - span_s;
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
                    context.fill_rect(x, y - h, COLUMN, h);
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
    };
    view! {
        <section class="panel history">
            <h2>"cores in use" <small>{format!("{label}, stacked by user · queue in tasks")}</small></h2>
            <Surface draw=draw/>
        </section>
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
        context.fill_rect(x, floor - h.max(1.0), COLUMN, h.max(1.0));
    }
}

/// Time labels on whole hours or days in the browser's time zone, as often as they fit.
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
    let every_h = LABEL_EVERY_H
        .into_iter()
        .find(|hours| {
            #[expect(clippy::cast_precision_loss, reason = "a few hundred hours")]
            let hours = *hours as f64;
            hours * 3_600.0 * per_second >= LABEL_GAP
        })
        .unwrap_or(336);
    context.set_fill_style_str(&color.css());
    let mut last = f64::NEG_INFINITY;
    let mut hour = from.div_euclid(3_600) * 3_600 + 3_600;
    while hour < from + span_s {
        let local = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(
            #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
            {
                hour as f64 * 1_000.0
            },
        ));
        let local_hour = i64::from(local.get_hours());
        if local_hour % every_h.min(24) == 0 && (every_h < 24 || local_hour == 0) {
            let text = if every_h >= 24 {
                clock(UnixTime(hour), true)
                    .split(' ')
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                clock(UnixTime(hour), false)[..5].to_owned()
            };
            #[expect(clippy::cast_precision_loss, reason = "seconds fit in an f64")]
            let x = LABELS + (hour - from) as f64 * per_second;
            if x - last >= LABEL_GAP {
                let _ = context.fill_text(&text, x, height - 4.0);
                last = x;
            }
        }
        hour += 3_600;
    }
}
