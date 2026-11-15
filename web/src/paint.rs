//! Colors from the theme, a canvas that redraws when its panel resizes, and clock times.

use leptos::html;
use leptos::prelude::*;
use rtviz_model::theme::{Color, Theme};
use rtviz_model::{Cluster, Login, UnixTime};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{CanvasRenderingContext2d, ResizeObserver, ResizeObserverEntry};

/// A color for canvas drawing, as red, green and blue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// The theme color `color`.
    pub fn of(color: &Color) -> Self {
        let (r, g, b) = color.rgb();
        Self(r, g, b)
    }

    /// This color moved toward `toward`, keeping `keep` of itself.
    pub fn fade(self, toward: Self, keep: f64) -> Self {
        let channel = |from: u8, to: u8| {
            let mixed = f64::from(from) * keep + f64::from(to) * (1.0 - keep);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to a channel"
            )]
            let channel = mixed.round().clamp(0.0, 255.0) as u8;
            channel
        };
        Self(
            channel(self.0, toward.0),
            channel(self.1, toward.1),
            channel(self.2, toward.2),
        )
    }

    /// The color as CSS.
    pub fn css(self) -> String {
        format!("rgb({},{},{})", self.0, self.1, self.2)
    }
}

/// The color of `login`, from the palette slot the server gave it, or the muted color for someone
/// not seen yet.
pub fn user_color(theme: &Theme, cluster: &Cluster, login: &Login) -> Rgb {
    let slot = cluster.users.get(login).map(|user| user.color_slot);
    slot.and_then(|slot| {
        let index = usize::try_from(slot).ok()? % theme.users.len().max(1);
        theme.users.get(index)
    })
    .map_or_else(|| Rgb::of(&theme.ui.muted), Rgb::of)
}

/// How to show `login`: the full name when the server sends one.
pub fn display_name(cluster: &Cluster, login: &Login) -> String {
    cluster
        .users
        .get(login)
        .and_then(|user| user.name.clone())
        .unwrap_or_else(|| login.0.clone())
}

/// A canvas that fills its panel and calls `draw` with the context and its size in CSS pixels whenever
/// the panel resizes, the fonts finish loading, or a signal `draw` reads changes.
#[component]
pub fn Surface<F>(draw: F) -> impl IntoView
where
    F: Fn(&CanvasRenderingContext2d, f64, f64) + 'static,
{
    let frame = NodeRef::<html::Div>::new();
    let canvas = NodeRef::<html::Canvas>::new();
    let size = RwSignal::new((0.0, 0.0));
    let fonts = RwSignal::new(false);
    frame.on_load(move |frame| {
        let resized = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
            let entry: ResizeObserverEntry = entries.get(0).unchecked_into();
            let rect = entry.content_rect();
            size.try_set((rect.width(), rect.height()));
        });
        let Ok(observer) = ResizeObserver::new(resized.as_ref().unchecked_ref()) else {
            return;
        };
        observer.observe(&frame);
        resized.forget();
        let observer = StoredValue::new_local(observer);
        on_cleanup(move || observer.with_value(ResizeObserver::disconnect));
    });
    leptos::task::spawn_local(async move {
        if let Ok(ready) = document().fonts().ready() {
            let _ = wasm_bindgen_futures::JsFuture::from(ready).await;
        }
        fonts.try_set(true);
    });
    Effect::new(move |_| {
        let (width, height) = size.get();
        fonts.track();
        let Some(canvas) = canvas.get() else {
            return;
        };
        let ratio = window().device_pixel_ratio();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a canvas is far smaller than u32::MAX"
        )]
        let pixels = |length: f64| (length * ratio).round().max(1.0) as u32;
        canvas.set_width(pixels(width));
        canvas.set_height(pixels(height));
        let Some(context) = canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|context| context.dyn_into::<CanvasRenderingContext2d>().ok())
        else {
            return;
        };
        let _ = context.set_transform(ratio, 0.0, 0.0, ratio, 0.0, 0.0);
        context.clear_rect(0.0, 0.0, width, height);
        draw(&context, width, height);
    });
    view! {
        <div class="surface" node_ref=frame>
            <canvas node_ref=canvas></canvas>
        </div>
    }
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `at` in the browser's time zone, as `14:05:09`, or as `Sat 14 Nov 14:05:09` with `date`.
pub fn clock(at: UnixTime, date: bool) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "seconds since 1970 fit in an f64"
    )]
    let time = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at.0 as f64 * 1_000.0));
    let hms = format!(
        "{:02}:{:02}:{:02}",
        time.get_hours(),
        time.get_minutes(),
        time.get_seconds()
    );
    if !date {
        return hms;
    }
    let day = DAYS[time.get_day() as usize % 7];
    let month = MONTHS[time.get_month() as usize % 12];
    format!("{day} {} {month} {hms}", time.get_date())
}

/// The browser's clock.
pub fn now() -> UnixTime {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "milliseconds since 1970 fit in an i64"
    )]
    let seconds = (js_sys::Date::now() / 1_000.0) as i64;
    UnixTime(seconds)
}
