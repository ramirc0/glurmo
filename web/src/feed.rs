//! What the server sends: the live stream, the themes and layouts, and usage history.

use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rtviz_model::usage::{Step, Usage, step_for};
use rtviz_model::{Cluster, Delta};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{EventSource, MessageEvent};

use crate::state::{Link, State};

/// How long to wait before opening a new stream after the browser gave up on one.
const REOPEN: Duration = Duration::from_secs(5);

/// How often usage history is read again.
const USAGE_EVERY: Duration = Duration::from_secs(60);

/// The first event of a stream: the site and the whole cluster.
#[derive(Deserialize)]
struct Snapshot {
    site: Site,
    #[serde(flatten)]
    cluster: Cluster,
}

/// The part of the server's site facts the page shows.
#[derive(Deserialize)]
struct Site {
    cluster: String,
}

/// Follows the server's stream into `state`. The browser reconnects a dropped stream on its own and
/// sends `Last-Event-ID`, so the server resumes with the deltas it missed.
pub fn follow(state: State) {
    let Ok(source) = EventSource::new("/api/events") else {
        web_sys::console::error_1(&"this browser cannot open an event stream".into());
        return;
    };
    listen(&source, "snapshot", move |data| {
        let snapshot: Snapshot = serde_json::from_str(data)?;
        state.cluster_name.set(snapshot.site.cluster);
        state.cluster.set(snapshot.cluster);
        count("snapshots");
        Ok(())
    });
    listen(&source, "delta", move |data| {
        let delta: Delta = serde_json::from_str(data)?;
        state
            .cluster
            .with_untracked(|cluster| state.ticker.update(|ticker| ticker.record(cluster, &delta)));
        state.cluster.update(|cluster| cluster.apply(&delta));
        count("deltas");
        Ok(())
    });
    listen(&source, "library", move |_| {
        load_library(state);
        Ok(())
    });
    let opened = Closure::<dyn FnMut()>::new(move || {
        if state.link.get_untracked() != Link::Connecting {
            count("reconnects");
        }
        state.link.set(Link::Live);
    });
    source.set_onopen(Some(opened.as_ref().unchecked_ref()));
    opened.forget();
    let target = source.clone();
    let failed = Closure::<dyn FnMut()>::new(move || {
        if !matches!(state.link.get_untracked(), Link::Lost(_)) {
            state.link.set(Link::Lost(js_sys::Date::now()));
        }
        if target.ready_state() == EventSource::CLOSED {
            set_timeout(move || follow(state), REOPEN);
        }
    });
    source.set_onerror(Some(failed.as_ref().unchecked_ref()));
    failed.forget();
}

/// Calls `handle` with the data of each `kind` event, and logs what it cannot read.
fn listen(
    source: &EventSource,
    kind: &'static str,
    handle: impl Fn(&str) -> serde_json::Result<()> + 'static,
) {
    let closure = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let data = event.data().as_string().unwrap_or_default();
        if let Err(error) = handle(&data) {
            web_sys::console::error_1(&format!("cannot read a {kind} event: {error}").into());
        }
    });
    if source
        .add_event_listener_with_callback(kind, closure.as_ref().unchecked_ref())
        .is_err()
    {
        web_sys::console::error_1(&format!("cannot listen for {kind} events").into());
    }
    closure.forget();
}

/// Adds one to a counter on `window.rtviz`, which the headless check reads.
fn count(name: &str) {
    let window = window();
    let key = wasm_bindgen::JsValue::from_str("rtviz");
    let counters = js_sys::Reflect::get(&window, &key)
        .ok()
        .filter(wasm_bindgen::JsValue::is_object)
        .unwrap_or_else(|| {
            let counters = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&window, &key, &counters);
            counters.into()
        });
    let name = wasm_bindgen::JsValue::from_str(name);
    let value = js_sys::Reflect::get(&counters, &name)
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let _ = js_sys::Reflect::set(&counters, &name, &(value + 1.0).into());
}

/// Counts each font that finishes loading in `state`. A canvas asks for a font only when it draws, so
/// it draws once in a fallback and again when the font arrives.
pub fn watch_fonts(state: State) {
    let loaded = Closure::<dyn FnMut()>::new(move || state.fonts.update(|count| *count += 1));
    if document()
        .fonts()
        .add_event_listener_with_callback("loadingdone", loaded.as_ref().unchecked_ref())
        .is_err()
    {
        web_sys::console::error_1(&"cannot watch fonts load".into());
    }
    loaded.forget();
}

/// Reads the themes and layouts into `state`.
pub fn load_library(state: State) {
    spawn_local(async move {
        match fetch("/api/themes").await {
            Ok(themes) => state.themes.set(themes),
            Err(error) => web_sys::console::error_1(&format!("no themes: {error}").into()),
        }
        match fetch("/api/layouts").await {
            Ok(layouts) => state.layouts.set(layouts),
            Err(error) => web_sys::console::error_1(&format!("no layouts: {error}").into()),
        }
    });
}

/// Reads usage history into `state` now, every minute, and whenever the layout's history span changes.
pub fn watch_usage(state: State) {
    let ticks = RwSignal::new(0_u64);
    set_interval(move || ticks.update(|count| *count += 1), USAGE_EVERY);
    Effect::new(move |_| {
        ticks.track();
        let span_s = state.history_span_s();
        spawn_local(async move { state.usage.set(usage(span_s).await) });
    });
}

/// Usage by login over the last `span_s` seconds.
async fn usage(span_s: i64) -> Result<Usage, String> {
    let to = crate::paint::now().0;
    let from = to - span_s;
    let step_s = step_for(span_s);
    let url = format!("/api/usage?from={from}&to={to}&step={step_s}&by=login");
    let rows: Vec<Step> = fetch(&url).await?;
    Ok(Usage {
        from: rtviz_model::UnixTime(from),
        to: rtviz_model::UnixTime(to),
        step_s,
        steps: rows,
    })
}

/// Reads JSON from `url` on the server.
async fn fetch<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let response = gloo_net::http::Request::get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("{url} answered {}", response.status()));
    }
    response.json().await.map_err(|error| error.to_string())
}
