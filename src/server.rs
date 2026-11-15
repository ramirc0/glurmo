//! The HTTP server: the live stream as server-sent events, snapshots, usage history, themes, layouts
//! and the sampler's health.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::to_ron;
use crate::hub::{EventId, Hub, Message};
use crate::library::Library;
use crate::sampler::Health;
use crate::store::{By, History, UsageQuery};
use rtviz_model::layout::Layout;
use rtviz_model::theme::Theme;
use rtviz_model::{Outage, UnixTime};

/// How long a browser waits before it reconnects a dropped stream.
const RETRY: Duration = Duration::from_secs(5);

/// How often an idle stream sends a comment, so proxies keep it open.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// The most steps one usage query may return per key.
const MAX_STEPS: i64 = 10_000;

/// What every handler shares.
#[derive(Clone)]
struct App {
    hub: Hub,
    history: Arc<History>,
    library: watch::Receiver<Library>,
}

/// The routes, serving `hub`'s stream, `history`'s usage and `library`'s themes and layouts.
pub fn router(hub: Hub, history: History, library: watch::Receiver<Library>) -> Router {
    let app = App {
        hub,
        history: Arc::new(history),
        library,
    };
    Router::new()
        .route("/api/events", get(events))
        .route("/api/snapshot", get(snapshot))
        .route("/api/usage", get(usage))
        .route("/api/themes", get(themes))
        .route("/api/themes/{name}", get(theme))
        .route("/theme.css", get(theme_css))
        .route("/api/layouts", get(layouts))
        .route("/api/layouts/{name}", get(layout))
        .route("/api/ron", post(ron))
        .route("/api/health", get(health))
        .fallback(asset)
        .with_state(app)
}

/// Serves `router` on `listen` until the process ends.
///
/// # Errors
///
/// Fails when the address cannot be bound or the server stops.
pub async fn serve(listen: SocketAddr, router: Router) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    axum::serve(listener, router).await?;
    Ok(())
}

/// An error as an HTTP status and a line of text.
struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

fn not_found(kind: &str, name: &str) -> Failure {
    Failure(StatusCode::NOT_FOUND, format!("no {kind} named {name:?}"))
}

/// The live stream. A client without a usable `Last-Event-ID` starts with a `snapshot` event, then gets
/// one `delta` event per tick. A `library` event names the themes and layouts after a file changes.
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    let last = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<EventId>().ok());
    let mut follow = app.hub.follow(last);
    let mut library = app.library.clone();
    library.mark_unchanged();
    let boot = app.hub.boot();
    let (sender, receiver) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut retry = Some(RETRY);
        loop {
            let event = tokio::select! {
                message = follow.next() => match message {
                    Some(message) => sse_event(boot, &message),
                    None => break,
                },
                Ok(()) = library.changed() => {
                    let names = Names::of(&library.borrow_and_update());
                    sse::Event::default().event("library").json_data(names)
                }
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    eprintln!("cannot write an event: {error}");
                    break;
                }
            };
            let event = match retry.take() {
                Some(retry) => event.retry(retry),
                None => event,
            };
            if sender.send(Ok(event)).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::new().interval(KEEPALIVE))
}

fn sse_event(boot: UnixTime, message: &Message) -> Result<sse::Event, axum::Error> {
    let id = EventId {
        boot,
        seq: message.seq(),
    };
    let event = sse::Event::default().id(id.to_string());
    match message {
        Message::Snapshot(snapshot) => event.event("snapshot").json_data(snapshot),
        Message::Delta(delta) => event.event("delta").json_data(&**delta),
    }
}

/// The names of the themes and layouts.
#[derive(Serialize)]
struct Names {
    themes: Vec<String>,
    layouts: Vec<String>,
}

impl Names {
    fn of(library: &Library) -> Self {
        Self {
            themes: library.themes.keys().cloned().collect(),
            layouts: library.layouts.keys().cloned().collect(),
        }
    }
}

/// The whole cluster as of the latest delta.
async fn snapshot(State(app): State<App>) -> Json<crate::hub::Snapshot> {
    Json(app.hub.snapshot())
}

/// The usage query string. Times are Unix seconds. It covers the last day in five-minute steps by login
/// unless it says otherwise.
#[derive(Deserialize)]
struct UsageParams {
    from: Option<i64>,
    to: Option<i64>,
    step: Option<i64>,
    by: Option<By>,
}

/// Usage summed per step. Failures come back only by partition.
async fn usage(
    State(app): State<App>,
    Query(params): Query<UsageParams>,
) -> Result<Json<Vec<rtviz_model::usage::Step>>, Failure> {
    let now = UnixTime::now();
    let to = params.to.unwrap_or(now.0);
    let query = UsageQuery {
        from: UnixTime(params.from.unwrap_or(to - 86_400)),
        to: UnixTime(to),
        step_s: params.step.unwrap_or(300).max(1),
        by: params.by.unwrap_or(By::Login),
    };
    if query.to <= query.from || (query.to.0 - query.from.0) / query.step_s > MAX_STEPS {
        return Err(Failure(
            StatusCode::BAD_REQUEST,
            format!("from must come before to, with at most {MAX_STEPS} steps between"),
        ));
    }
    let steps = tokio::task::spawn_blocking(move || app.history.usage(&query, now))
        .await
        .context("the usage query panicked")??;
    Ok(Json(steps))
}

async fn themes(State(app): State<App>) -> Json<std::collections::BTreeMap<String, Theme>> {
    Json(app.library.borrow().themes.clone())
}

async fn theme(State(app): State<App>, Path(name): Path<String>) -> Result<Json<Theme>, Failure> {
    let library = app.library.borrow();
    let theme = library
        .themes
        .get(&name)
        .ok_or_else(|| not_found("theme", &name))?;
    Ok(Json(theme.clone()))
}

#[derive(Deserialize)]
struct CssParams {
    name: Option<String>,
}

/// A theme as CSS variables, `rose-pine` unless `name` says otherwise.
async fn theme_css(
    State(app): State<App>,
    Query(params): Query<CssParams>,
) -> Result<impl IntoResponse, Failure> {
    let name = params.name.unwrap_or_else(|| "rose-pine".to_owned());
    let library = app.library.borrow();
    let theme = library
        .themes
        .get(&name)
        .ok_or_else(|| not_found("theme", &name))?;
    Ok(([(header::CONTENT_TYPE, "text/css")], theme.css()))
}

async fn layouts(State(app): State<App>) -> Json<std::collections::BTreeMap<String, Layout>> {
    Json(app.library.borrow().layouts.clone())
}

async fn layout(State(app): State<App>, Path(name): Path<String>) -> Result<Json<Layout>, Failure> {
    let library = app.library.borrow();
    let layout = library
        .layouts
        .get(&name)
        .ok_or_else(|| not_found("layout", &name))?;
    Ok(Json(layout.clone()))
}

/// A theme or layout to write as RON.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Export {
    Theme(Box<Theme>),
    Layout(Layout),
}

/// Writes a theme or layout, sent as `{"theme": ...}` or `{"layout": ...}`, as RON to save in a file.
/// It changes nothing on the server.
async fn ron(Json(export): Json<Export>) -> Result<String, Failure> {
    let ron = match export {
        Export::Theme(theme) => to_ron(&theme)?,
        Export::Layout(layout) => to_ron(&layout)?,
    };
    Ok(ron)
}

/// The sampler's state for monitoring.
#[derive(Serialize)]
struct Status {
    boot: UnixTime,
    at: Option<UnixTime>,
    outage: Option<Outage>,
    commands: Health,
}

async fn health(State(app): State<App>) -> Json<Status> {
    let (at, outage) = app.hub.read(|cluster| (cluster.at, cluster.outage.clone()));
    Json(Status {
        boot: app.hub.boot(),
        at,
        outage,
        commands: app.hub.health(),
    })
}

/// The web view as trunk builds it. A build without it serves the API alone.
#[derive(RustEmbed)]
#[folder = "web/dist"]
#[allow_missing = true]
struct Assets;

/// A file of the web view, with `/` as `index.html`.
async fn asset(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        path => path,
    };
    let Some(file) = Assets::get(path) else {
        return not_found("file", path).into_response();
    };
    let headers = [
        (header::CONTENT_TYPE, content_type(path)),
        (header::CACHE_CONTROL, "no-cache"),
    ];
    (headers, file.data).into_response()
}

/// The content type of a file trunk writes, by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map_or("", |(_, extension)| extension) {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "wasm" => "application/wasm",
        "css" => "text/css",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slurm::Site;
    use crate::store::{Settings, Store};
    use axum::body::Body;
    use axum::http::Request;
    use rtviz_model::{Event, Stats};
    use tokio_stream::StreamExt;
    use tower::ServiceExt;

    /// A router over a hub with one delta, an empty store and the built-in library.
    fn app(dir: &tempfile::TempDir) -> (Router, Hub) {
        let path = dir.path().join("rtviz.sqlite");
        let settings = Settings {
            backfill_days: 0,
            keep_days: None,
            backup: dir.path().join("backup.sqlite"),
        };
        Store::open(&path, settings)
            .unwrap()
            .sampled(UnixTime::now(), 300)
            .unwrap();
        let hub = Hub::new(Site::zlurm());
        hub.publish(UnixTime::now(), vec![Event::Stats(Stats::default())]);
        let library = watch::channel(Library::load(dir.path())).1;
        (router(hub.clone(), History::open(&path), library), hub)
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    async fn get(router: &Router, uri: &str) -> (StatusCode, HeaderMap, String) {
        send(router, Request::get(uri).body(Body::empty()).unwrap()).await
    }

    /// The first event on the stream, sent with `last` as `Last-Event-ID` if there is one.
    async fn first_event(router: &Router, last: Option<&str>) -> String {
        let mut request = Request::get("/api/events");
        if let Some(last) = last {
            request = request.header("last-event-id", last);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut stream = response.into_body().into_data_stream();
        let mut text = String::new();
        while !text.contains("\n\n") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("an event within 5 s")
                .unwrap()
                .unwrap();
            text.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        text
    }

    #[tokio::test]
    async fn a_new_stream_starts_with_a_snapshot_and_a_resumed_one_with_the_next_delta() {
        let dir = tempfile::tempdir().unwrap();
        let (router, hub) = app(&dir);
        let boot = hub.boot().0;
        let fresh = first_event(&router, None).await;
        assert!(fresh.contains("event: snapshot\n"), "{fresh}");
        assert!(fresh.contains(&format!("id: {boot}-1\n")), "{fresh}");
        assert!(fresh.contains("retry: 5000\n"), "{fresh}");

        let resumed = first_event(&router, Some(&format!("{boot}-0"))).await;
        assert!(resumed.contains("event: delta\n"), "{resumed}");
        assert!(resumed.contains(&format!("id: {boot}-1\n")), "{resumed}");

        let stale = first_event(&router, Some("12-0")).await;
        assert!(stale.contains("event: snapshot\n"), "an old boot: {stale}");
    }

    #[tokio::test]
    async fn snapshot_and_health_describe_the_hub() {
        let dir = tempfile::tempdir().unwrap();
        let (router, hub) = app(&dir);
        let (status, _, body) = get(&router, "/api/snapshot").await;
        assert_eq!(status, StatusCode::OK);
        let snapshot: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(snapshot["seq"], 1);
        assert_eq!(snapshot["site"]["cluster"], "zlurm");

        let (_, _, body) = get(&router, "/api/health").await;
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(health["boot"], hub.boot().0);
        assert_eq!(health["outage"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn usage_answers_and_rejects_bad_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let (router, _) = app(&dir);
        let (status, _, body) = get(&router, "/api/usage?by=partition").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, "[]");
        for bad in [
            "/api/usage?from=10&to=5",
            "/api/usage?from=0&to=100000000&step=300",
            "/api/usage?by=person",
        ] {
            let (status, _, _) = get(&router, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn themes_come_as_json_and_css() {
        let dir = tempfile::tempdir().unwrap();
        let (router, _) = app(&dir);
        let (_, _, body) = get(&router, "/api/themes").await;
        let themes: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(themes["rose-pine-dawn"]["ui"]["base"], "#faf4ed");

        let (status, headers, css) = get(&router, "/theme.css?name=rose-pine-moon").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/css");
        assert!(css.contains("--base: #232136;"));

        let (status, _, _) = get(&router, "/api/themes/neon").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn files_the_web_view_lacks_are_not_found_and_routes_still_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (router, _) = app(&dir);
        let (status, _, _) = get(&router, "/no-such-file.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = get(&router, "/api/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type("rtviz-web-1a2b_bg.wasm"), "application/wasm");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
    }

    #[tokio::test]
    async fn a_layout_round_trips_through_ron() {
        let dir = tempfile::tempdir().unwrap();
        let (router, _) = app(&dir);
        let (_, _, json) = get(&router, "/api/layouts/tui").await;
        let request = Request::post("/api/ron")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"layout": {json}}}"#)))
            .unwrap();
        let (status, _, ron) = send(&router, request).await;
        assert_eq!(status, StatusCode::OK, "{ron}");
        assert!(ron.starts_with("Layout("), "{ron}");
        let parsed: Layout = crate::config::from_ron(&ron).unwrap();
        assert_eq!(parsed, Library::load(dir.path()).layouts["tui"]);
    }
}
