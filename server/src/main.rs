mod store;

use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Multipart, Path, Query, Request, State, WebSocketUpgrade,
        multipart::MultipartError,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use maggotdrop_protocol::{BUFFER_BYTES, EVENTS_PATH, Event, FILES_PATH, UploadedFile};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use store::{PAYLOAD, Store};
use subtle::ConstantTimeEq;
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    net::TcpListener,
    time::{interval, timeout},
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const PAGE_SIZE: usize = 100;
const COOKIE: &str = "maggotdrop_session";
const WEB_REQUEST: &str = "x-maggotdrop-request";

struct App {
    store: Store,
    key: String,
    web_token: String,
    secure_cookie: bool,
    max_upload_bytes: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let key = env::var("MAGGOTDROP_KEY").context("set MAGGOTDROP_KEY")?;
    ensure!(
        !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_graphic()),
        "MAGGOTDROP_KEY must contain visible ASCII characters without spaces"
    );
    let bind: SocketAddr = env::var("MAGGOTDROP_BIND")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse()
        .context("invalid MAGGOTDROP_BIND")?;
    let root =
        env::var_os("MAGGOTDROP_DATA_DIR").map_or_else(|| PathBuf::from("data"), PathBuf::from);
    let max_upload_bytes = env::var("MAGGOTDROP_MAX_UPLOAD_BYTES")
        .ok()
        .map(|value| value.parse())
        .transpose()
        .context("invalid MAGGOTDROP_MAX_UPLOAD_BYTES")?
        .unwrap_or(DEFAULT_MAX_BYTES);
    let request_limit = usize::try_from(
        max_upload_bytes
            .checked_add(BUFFER_BYTES as u64)
            .context("upload limit out of range")?,
    )
    .context("upload limit out of range")?;
    let app = Arc::new(App {
        store: Store::open(&root).with_context(|| format!("opening {}", root.display()))?,
        key,
        web_token: Uuid::new_v4().simple().to_string(),
        secure_cookie: env::var("MAGGOTDROP_SECURE_COOKIE").as_deref() == Ok("1"),
        max_upload_bytes,
    });

    let protected = Router::new()
        .route(
            FILES_PATH,
            get(list_files)
                .post(upload)
                .layer(DefaultBodyLimit::max(request_limit)),
        )
        .route("/api/files/{id}", get(download))
        .route(EVENTS_PATH, get(events))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize));
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/app.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route("/api/session", post(login).delete(logout))
        .merge(protected)
        .layer(middleware::from_fn(response_headers))
        .with_state(app);

    let listener = TcpListener::bind(bind)
        .await
        .context("binding HTTP listener")?;
    eprintln!("MaggotDrop listening at http://{}", listener.local_addr()?);
    axum::serve(listener, router).await?;
    Ok(())
}

async fn response_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    ));
    response
}

fn key_matches(headers: &HeaderMap, app: &App) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, key)| scheme.eq_ignore_ascii_case("bearer") && equal(key, &app.key))
}

fn equal(left: &str, right: &str) -> bool {
    bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

fn web_request(headers: &HeaderMap) -> bool {
    headers.get(WEB_REQUEST).is_some_and(|value| value == "1")
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    if key_matches(headers, &app) {
        return next.run(request).await;
    }
    if request.uri().path() != EVENTS_PATH
        && (matches!(*request.method(), Method::GET | Method::HEAD) || web_request(headers))
        && headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .any(|(name, value)| name == COOKIE && equal(value, &app.web_token))
    {
        return next.run(request).await;
    }
    (StatusCode::UNAUTHORIZED, "KEY required or invalid").into_response()
}

async fn login(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !key_matches(&headers, &app) {
        return (StatusCode::UNAUTHORIZED, "KEY required or invalid").into_response();
    }
    session_cookie(&app, &app.web_token)
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !web_request(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    session_cookie(&app, "")
}

fn session_cookie(app: &App, value: &str) -> Response {
    let expiry = if value.is_empty() { "; Max-Age=0" } else { "" };
    let secure = if app.secure_cookie { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}={value}; Path=/api; HttpOnly; SameSite=Strict{expiry}{secure}");
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<u64>,
}

#[derive(Serialize)]
struct FilePage {
    files: Vec<UploadedFile>,
    next_before: Option<u64>,
}

async fn list_files(State(app): State<Arc<App>>, Query(query): Query<PageQuery>) -> Json<FilePage> {
    let mut files = app.store.list(query.before, PAGE_SIZE + 1);
    let next_before = if files.len() > PAGE_SIZE {
        files.pop();
        files.last().map(|file| file.sequence)
    } else {
        None
    };
    Json(FilePage { files, next_before })
}

type ApiError = (StatusCode, String);

fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("Storage error: {error}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Storage error".into())
}

fn multipart_error(error: MultipartError) -> ApiError {
    (error.status(), error.body_text())
}

fn normalize_name(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = basename.chars().filter(|ch| !ch.is_control()).collect();
    match name.trim() {
        "" | "." | ".." => "file".into(),
        usable => usable.into(),
    }
}

async fn upload(
    State(app): State<Arc<App>>,
    mut form: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    let bad_form = || {
        (
            StatusCode::BAD_REQUEST,
            "Send exactly one file in the 'file' field".into(),
        )
    };
    let mut field = form
        .next_field()
        .await
        .map_err(multipart_error)?
        .ok_or_else(bad_form)?;
    if field.name() != Some("file") {
        return Err(bad_form());
    }
    let name = normalize_name(
        field
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(bad_form)?,
    );
    if name.len() > 1024 {
        return Err((StatusCode::BAD_REQUEST, "Filename is too long".into()));
    }
    let stage = app.store.stage().map_err(internal)?;
    let file = File::create(stage.path().join(PAYLOAD))
        .await
        .map_err(internal)?;
    let mut output = BufWriter::with_capacity(BUFFER_BYTES, file);
    let mut size = 0u64;
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        size = size.saturating_add(chunk.len() as u64);
        if size > app.max_upload_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "File exceeds the upload limit".into(),
            ));
        }
        output.write_all(&chunk).await.map_err(internal)?;
    }
    drop(field);
    if form.next_field().await.map_err(multipart_error)?.is_some() {
        return Err(bad_form());
    }
    output.flush().await.map_err(internal)?;
    output.into_inner().sync_all().await.map_err(internal)?;
    let file = tokio::task::spawn_blocking(move || app.store.commit(stage, name, size))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(file)))
}

async fn download(State(app): State<Arc<App>>, Path(id): Path<Uuid>) -> Result<Response, ApiError> {
    let (entry, path) = app
        .store
        .get(id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "File not found".into()))?;
    let file = File::open(path).await.map_err(internal)?;
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        utf8_percent_encode(&entry.name, NON_ALPHANUMERIC)
    );
    let headers = [
        (header::CONTENT_TYPE, "application/octet-stream".into()),
        (header::CONTENT_LENGTH, entry.size.to_string()),
        (header::CONTENT_DISPOSITION, disposition),
    ];
    Ok((
        headers,
        Body::from_stream(ReaderStream::with_capacity(file, BUFFER_BYTES)),
    )
        .into_response())
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<u64>,
}

async fn events(
    State(app): State<Arc<App>>,
    Query(query): Query<EventsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Capture the baseline at handshake time, including uploads made before the socket task runs.
    let mut updates = app.store.subscribe();
    let latest = *updates.borrow_and_update();
    let after = query.after.unwrap_or(latest).min(latest);
    upgrade
        .max_message_size(4096)
        .max_frame_size(4096)
        .write_buffer_size(0)
        .max_write_buffer_size(128 * 1024)
        .on_upgrade(move |socket| event_stream(socket, app, updates, after))
}

async fn event_stream(
    mut socket: WebSocket,
    app: Arc<App>,
    mut updates: tokio::sync::watch::Receiver<u64>,
    mut cursor: u64,
) {
    if !send_event(&mut socket, &Event::Ready { after: cursor }).await {
        return;
    }
    let mut heartbeat = interval(Duration::from_secs(20));
    let mut last_proof = Instant::now();
    loop {
        let latest = *updates.borrow_and_update();
        if latest > cursor {
            for file in app.store.after(cursor, 32) {
                cursor = file.sequence;
                if !send_event(&mut socket, &Event::FileUploaded { file }).await {
                    return;
                }
            }
        }
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Pong(_) | Message::Ping(_))) => last_proof = Instant::now(),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                _ => {}
            },
            _ = heartbeat.tick() => {
                if last_proof.elapsed() >= Duration::from_secs(60)
                    || !send_message(&mut socket, Message::Ping(Default::default())).await
                {
                    return;
                }
            }
            _ = updates.changed(), if latest <= cursor => {}
            _ = tokio::task::yield_now(), if latest > cursor => {}
        }
    }
}

async fn send_event(socket: &mut WebSocket, event: &Event) -> bool {
    match serde_json::to_string(event) {
        Ok(json) => send_message(socket, Message::Text(json.into())).await,
        Err(error) => {
            eprintln!("Could not serialize upload event: {error}");
            false
        }
    }
}

async fn send_message(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        timeout(Duration::from_secs(10), socket.send(message)).await,
        Ok(Ok(()))
    )
}
