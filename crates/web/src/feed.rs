//! What the server sends: the live stream, the themes and layouts, and usage history.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use glurmo_model::layout::Layout;
use glurmo_model::palette::Palette;
use glurmo_model::theme::Theme;
use glurmo_model::usage::{Step, Usage, step_for};
use glurmo_model::{Cluster, Delta, UnixTime};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{EventSource, MessageEvent, RequestRedirect, ResponseType};

use crate::replay::update_live;
use crate::shelf::Shelf;
use crate::state::{Link, State};

/// How long to wait before opening a new stream after the browser gave up on one.
const REOPEN: Duration = Duration::from_secs(5);

/// How often usage history is read again.
const USAGE_EVERY: Duration = Duration::from_secs(60);

/// How long the timeline must rest before usage is read for it.
const SETTLE: Duration = Duration::from_millis(150);

/// Why a read from the server gave nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unread {
    /// The read was redirected, which glurmo never does. A login in front of it does once it has ended,
    /// and only loading the page again can sign in.
    SignedOut,
    /// Any other reason, in words.
    Failed(String),
}

impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignedOut => f.write_str("signed out, reload the page to sign in again"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

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
        update_live(state, |cluster, _| *cluster = snapshot.cluster);
        count("snapshots");
        Ok(())
    });
    listen(&source, "delta", move |data| {
        let delta: Delta = serde_json::from_str(data)?;
        update_live(state, |cluster, ticker| {
            ticker.record(cluster, &delta);
            cluster.apply(&delta);
        });
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

/// Adds one to a counter on `window.glurmo`, which the headless check reads.
fn count(name: &str) {
    let window = window();
    let key = wasm_bindgen::JsValue::from_str("glurmo");
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

/// Reads the server's themes, palettes and layouts into `state`, with the ones saved in this browser on
/// top.
pub fn load_library(state: State) {
    spawn_local(async move {
        match fetch::<BTreeMap<String, Theme>>("/api/themes").await {
            Ok(mut themes) => {
                themes.extend(Shelf::Themes.all());
                state.themes.try_set(themes);
            }
            Err(error) => web_sys::console::error_1(&format!("no themes: {error}").into()),
        }
        match fetch::<BTreeMap<String, Palette>>("/api/palettes").await {
            Ok(mut palettes) => {
                palettes.extend(Shelf::Palettes.all());
                state.palettes.try_set(palettes);
            }
            Err(error) => web_sys::console::error_1(&format!("no palettes: {error}").into()),
        }
        match fetch::<BTreeMap<String, Layout>>("/api/layouts").await {
            Ok(mut layouts) => {
                layouts.extend(Shelf::Layouts.all());
                state.layouts.try_set(layouts);
            }
            Err(error) => web_sys::console::error_1(&format!("no layouts: {error}").into()),
        }
    });
}

/// Reads usage history into `state` now, every minute, and whenever the timeline moves. It reads a span
/// either side of the one shown, so a pan draws at once. A burst of moves reads once, after it settles.
pub fn watch_usage(state: State) {
    let ticks = RwSignal::new(0_u64);
    set_interval(move || ticks.update(|count| *count += 1), USAGE_EVERY);
    let span_s = Memo::new(move |_| state.history_span_s());
    let end = Memo::new(move |_| state.timeline.with(|timeline| timeline.end));
    let asked = StoredValue::new(0_u64);
    Effect::new(move |_| {
        ticks.track();
        let (span_s, end) = (span_s.get(), end.get());
        asked.update_value(|asked| *asked += 1);
        let ask = asked.get_value();
        set_timeout(
            move || {
                if asked.try_get_value() != Some(ask) {
                    return;
                }
                spawn_local(async move {
                    let usage = usage(span_s, end).await;
                    if asked.try_get_value() == Some(ask) {
                        state.usage.try_set(usage);
                    }
                });
            },
            SETTLE,
        );
    });
}

/// Usage by login for a span of `span_s` ending at `end`, or now, with a span either side as far as now.
async fn usage(span_s: i64, end: Option<UnixTime>) -> Result<Usage, Unread> {
    let now = crate::paint::now().0;
    let end = end.map_or(now, |end| end.0.min(now));
    let from = end - 2 * span_s;
    let to = (end + span_s).min(now);
    let step_s = step_for(span_s);
    let url = format!("/api/usage?from={from}&to={to}&step={step_s}&by=login");
    let rows: Vec<Step> = fetch(&url).await?;
    Ok(Usage {
        from: UnixTime(from),
        to: UnixTime(to),
        step_s,
        steps: rows,
    })
}

/// Reads JSON from `url` on the server. A redirect is not followed, because it leads to a login page on
/// another site, which the browser refuses to read with an error that names no cause.
pub async fn fetch<T: DeserializeOwned>(url: &str) -> Result<T, Unread> {
    let response = gloo_net::http::Request::get(url)
        .redirect(RequestRedirect::Manual)
        .send()
        .await
        .map_err(|error| Unread::Failed(format!("cannot reach the server ({error})")))?;
    if response.type_() == ResponseType::Opaqueredirect {
        return Err(Unread::SignedOut);
    }
    if !response.ok() {
        return Err(Unread::Failed(format!(
            "{url} answered {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| Unread::Failed(error.to_string()))
}

/// The RON the server writes for `value`, a `kind` such as `layout`, to save in a file.
pub async fn ron<T: serde::Serialize>(kind: &str, value: &T) -> Result<String, String> {
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    let body = serde_json::Value::Object([(kind.to_owned(), value)].into_iter().collect());
    let response = gloo_net::http::Request::post("/api/ron")
        .json(&body)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let text = response.text().await.map_err(|error| error.to_string())?;
    if response.ok() {
        Ok(text)
    } else {
        Err(format!("the server answered {}: {text}", response.status()))
    }
}
