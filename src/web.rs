//! HTTP API, websocket and embedded static files.

use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::spawn_blocking;

use crate::adapter::adapter_info;
use crate::monitor::Monitor;
use crate::elevation::{self, ElevationParams, ElevationPreview};
use crate::db::{
    BackupInfo, Db, Person, PersonError, PersonUpdate, RestoreError, RideDetail, RideSummary, StrapError, inspect_backup,
};
use crate::program::{FlatSegment, Program};
use crate::protocol::DeviceLimits;
use crate::ride::{DeleteError, LiveView, RideManager, StartError, UiCommand};
use crate::strap::{Found, Link, ScanError, StrapState};

#[derive(Clone)]
pub struct AppState {
    pub rides: RideManager,
    pub db: Db,
    pub limits: DeviceLimits,
    /// Extra host names (lowercase, no port) accepted by the host allowlist.
    pub allowed_hosts: Vec<String>,
    /// Writable directory for backup and restore scratch files (next to the database).
    pub scratch_dir: PathBuf,
    pub service: ServiceInfo,
}

/// What the service page shows about the setup.
#[derive(Clone)]
pub struct ServiceInfo {
    pub monitor: Monitor,
    /// `KETTLER_DEVICE` as given.
    pub device_url: String,
    /// The port of a `serial://` device, for the adapter lookup.
    pub serial_port: Option<String>,
    /// `"device"`, `"tcp"` or `"off"`.
    pub keys: &'static str,
    pub db_path: PathBuf,
    pub started_at_ms: i64,
}

/// Version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit baked in at build time (`KETTLER_GIT_SHA`), "dev" when unset or empty.
pub fn commit() -> &'static str {
    match option_env!("KETTLER_GIT_SHA") {
        Some(sha) if !sha.trim().is_empty() => sha,
        _ => "dev",
    }
}

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/persons", get(list_persons).post(add_person))
        .route("/api/persons/{id}", delete(delete_person).put(update_person))
        .route("/api/persons/{id}/stats", get(person_stats))
        .route("/api/straps", get(list_straps).post(add_strap))
        .route("/api/straps/scan", post(scan_straps))
        .route("/api/straps/{id}", delete(delete_strap).put(rename_strap))
        .route("/api/programs", get(list_programs).post(save_program))
        .route("/api/programs/{id}", get(get_program).delete(delete_program))
        .route("/api/rides", get(list_rides).post(start_ride))
        .route("/api/rides/{id}", get(get_ride).delete(delete_ride))
        .route("/api/limits", get(limits))
        .route("/api/version", get(version))
        .route("/api/service/status", get(service_status))
        .route("/api/service/backup", get(download_backup))
        .route("/api/service/restore/check", post(check_restore).layer(DefaultBodyLimit::max(MAX_BACKUP_BYTES)))
        .route("/api/service/restore", post(restore).layer(DefaultBodyLimit::max(MAX_BACKUP_BYTES)))
        .route("/ws", get(ws))
        .route("/ws/service", get(ws_service))
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .with_state(state)
}

/// Serves until `shutdown` completes, then stops accepting and lets open requests finish.
pub async fn run(
    listen: SocketAddr,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(state)).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

/// Error with a German message, rendered as `{"error": ...}`.
struct ApiError(StatusCode, String);

impl ApiError {
    fn internal(e: impl std::fmt::Display) -> Self {
        tracing::error!("request failed: {e}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "Interner Fehler".into())
    }
}

impl From<StartError> for ApiError {
    fn from(e: StartError) -> Self {
        let status = match e {
            StartError::AlreadyRunning => StatusCode::CONFLICT,
            StartError::UnknownPerson | StartError::UnknownProgram => StatusCode::NOT_FOUND,
            StartError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

const BAD_REQUEST: &str = "Ungültige Anfrage";

fn rejection(status: StatusCode) -> ApiError {
    ApiError(status, BAD_REQUEST.into())
}

/// `Json`, `Query` and `Path` with rejections rendered as `{"error": ...}`.
struct ApiJson<T>(T);
struct ApiQuery<T>(T);
struct ApiPath<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(r) => Err(rejection(r.status())),
        }
    }
}

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(r) => Err(rejection(r.status())),
        }
    }
}

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(v)) => Ok(Self(v)),
            Err(r) => Err(rejection(r.status())),
        }
    }
}

/// Blocks cross-site requests that could drive the ergometer or read its data. For the
/// websocket and every API call the `Host` must be on the allowlist (against DNS rebinding).
/// For the websocket and state-changing API calls an `Origin` header, if present, must also
/// name the same host as `Host`. Requests without `Origin` (curl, same-origin GETs) pass that check.
async fn check_origin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let is_ws = path == "/ws" || path == "/ws/service";
    let is_api = path == "/api" || path.starts_with("/api/");
    if is_ws || is_api {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .or_else(|| req.uri().authority().map(|a| a.as_str()));
        if !host.is_some_and(|h| host_allowed(h, &state.allowed_hosts)) {
            return ApiError(StatusCode::FORBIDDEN, "Unbekannter Hostname".into()).into_response();
        }
        let writes = !matches!(*req.method(), Method::GET | Method::HEAD);
        if (is_ws || writes) && !origin_ok(req.headers(), host) {
            return ApiError(StatusCode::FORBIDDEN, "Fremde Herkunft abgelehnt".into()).into_response();
        }
    }
    next.run(req).await
}

/// Host part of a `Host` value without the port; IPv6 literals keep their brackets.
fn host_name(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |end| &host[..=end]);
    }
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

/// IP literals, `localhost`, dotless names, local-network suffixes and `extra` are allowed.
fn host_allowed(host: &str, extra: &[String]) -> bool {
    let name = host_name(host).to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    let ip = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')).unwrap_or(&name);
    ip.parse::<std::net::IpAddr>().is_ok()
        || name == "localhost"
        || !name.contains('.')
        || [".local", ".lan", ".home.arpa", ".fritz.box"].iter().any(|suffix| name.ends_with(suffix))
        || extra.contains(&name)
}

fn origin_ok(headers: &HeaderMap, host: Option<&str>) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest.trim_end_matches('/'));
    matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

type ApiResult<T> = Result<T, ApiError>;

/// Runs a blocking db call off the async threads.
async fn blocking<T: Send + 'static>(
    db: &Db,
    f: impl FnOnce(Db) -> anyhow::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let db = db.clone();
    spawn_blocking(move || f(db))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)
}

async fn list_persons(State(s): State<AppState>) -> ApiResult<Json<Vec<Person>>> {
    Ok(Json(blocking(&s.db, |db| db.list_persons()).await?))
}

#[derive(Deserialize)]
struct NewPerson {
    name: String,
}

async fn add_person(State(s): State<AppState>, ApiJson(p): ApiJson<NewPerson>) -> ApiResult<Json<serde_json::Value>> {
    let db = s.db.clone();
    let id = spawn_blocking(move || db.add_person(&p.name)).await.map_err(ApiError::internal)?.map_err(person_error)?;
    Ok(Json(json!({ "id": id })))
}

/// A `PersonError` or `StrapError` is the user's mistake (422), anything else an internal error.
fn person_error(e: anyhow::Error) -> ApiError {
    if let Some(invalid) = e.downcast_ref::<PersonError>() {
        return ApiError(StatusCode::UNPROCESSABLE_ENTITY, invalid.to_string());
    }
    match e.downcast_ref::<StrapError>() {
        Some(invalid) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, invalid.to_string()),
        None => ApiError::internal(e),
    }
}

/// A field that may be missing (`None`), `null` (`Some(None)`) or a value.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// Omitted fields keep their value; unknown fields are ignored.
#[derive(Deserialize)]
struct PersonChange {
    name: Option<String>,
    weight_kg: Option<f64>,
    flat_watts: Option<u16>,
    /// Missing keeps the strap, `null` clears it.
    #[serde(default, deserialize_with = "present")]
    strap_id: Option<Option<i64>>,
}

/// Changes name, weight or flat power; past rides keep their snapshot and follow a
/// rename because they only store the id.
async fn update_person(State(s): State<AppState>, ApiPath(id): ApiPath<i64>, ApiJson(p): ApiJson<PersonChange>) -> ApiResult<Json<Person>> {
    if p.name.is_none() && p.weight_kg.is_none() && p.flat_watts.is_none() && p.strap_id.is_none() {
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Keine Änderung angegeben".into()));
    }
    let update = PersonUpdate { name: p.name, weight_kg: p.weight_kg, flat_watts: p.flat_watts, strap_id: p.strap_id };
    let db = s.db.clone();
    let person = spawn_blocking(move || -> anyhow::Result<Option<Person>> {
        if !db.update_person(id, &update)? {
            return Ok(None);
        }
        Ok(db.list_persons()?.into_iter().find(|p| p.id == id))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(person_error)?;
    person.map(Json).ok_or_else(unknown_person)
}

async fn person_stats(State(s): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<serde_json::Value>> {
    let rides = blocking(&s.db, move |db| {
        db.list_persons()?.iter().any(|p| p.id == id).then(|| db.count_rides(id)).transpose()
    })
    .await?
    .ok_or_else(unknown_person)?;
    Ok(Json(json!({ "rides": rides })))
}

fn unknown_person() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "Unbekannte Person".into())
}

impl From<DeleteError> for ApiError {
    fn from(e: DeleteError) -> Self {
        let status = match e {
            DeleteError::RideRunning | DeleteError::PersonRiding => StatusCode::CONFLICT,
            DeleteError::UnknownRide | DeleteError::UnknownPerson => StatusCode::NOT_FOUND,
            DeleteError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, e.to_string())
    }
}

impl From<RestoreError> for ApiError {
    fn from(e: RestoreError) -> Self {
        match e {
            RestoreError::Invalid | RestoreError::TooNew => Self(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
            RestoreError::RideRunning => Self(StatusCode::CONFLICT, e.to_string()),
            RestoreError::Storage(_) => Self::internal(e),
        }
    }
}

/// Largest backup accepted for a restore.
const MAX_BACKUP_BYTES: usize = 256 * 1024 * 1024;

/// A scratch database file, removed with its `-wal` and `-shm` files on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new(dir: &FsPath) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(dir.join(format!(".kettler-tmp-{}-{n}.db", std::process::id())))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The registered straps with their state; without one from the backend they are out of range.
async fn list_straps(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let straps = blocking(&s.db, |db| db.list_straps()).await?;
    let hub = s.rides.straps();
    let states = hub.states();
    let list: Vec<StrapState> = straps
        .into_iter()
        .map(|strap| {
            states.iter().find(|st| st.id == strap.id).cloned().unwrap_or(StrapState {
                id: strap.id,
                name: strap.name,
                address: strap.address,
                link: Link::OutOfRange,
                pulse: None,
                battery: None,
                problem: None,
                pulse_at: None,
            })
        })
        .collect();
    Ok(Json(json!({ "availability": hub.availability(), "straps": list })))
}

/// The straps known to the backend follow the database.
async fn refresh_straps(s: &AppState) -> ApiResult<()> {
    let straps = blocking(&s.db, |db| db.list_straps()).await?;
    s.rides.straps().set_known(straps);
    Ok(())
}

#[derive(Serialize)]
struct FoundStrap {
    #[serde(flatten)]
    found: Found,
    known: bool,
}

async fn scan_straps(State(s): State<AppState>) -> ApiResult<Json<Vec<FoundStrap>>> {
    let found = s.rides.straps().scan().await.map_err(|e| {
        let status = match e {
            ScanError::Busy => StatusCode::CONFLICT,
            ScanError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ScanError::Failed(_) => StatusCode::BAD_GATEWAY,
        };
        ApiError(status, e.to_string())
    })?;
    let known = blocking(&s.db, |db| db.list_straps()).await?;
    Ok(Json(
        found
            .into_iter()
            .map(|f| FoundStrap { known: known.iter().any(|k| k.address.eq_ignore_ascii_case(&f.address)), found: f })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewStrap {
    address: String,
    name: String,
}

async fn add_strap(State(s): State<AppState>, ApiJson(n): ApiJson<NewStrap>) -> ApiResult<Json<serde_json::Value>> {
    let db = s.db.clone();
    let id = spawn_blocking(move || db.add_strap(&n.address, &n.name))
        .await
        .map_err(ApiError::internal)?
        .map_err(person_error)?;
    refresh_straps(&s).await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct StrapChange {
    name: String,
}

fn unknown_strap() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, StrapError::Unknown.to_string())
}

async fn rename_strap(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(c): ApiJson<StrapChange>,
) -> ApiResult<StatusCode> {
    let db = s.db.clone();
    let found = spawn_blocking(move || db.rename_strap(id, &c.name))
        .await
        .map_err(ApiError::internal)?
        .map_err(person_error)?;
    if !found {
        return Err(unknown_strap());
    }
    refresh_straps(&s).await?;
    Ok(StatusCode::OK)
}

/// Riders lose the assignment; recorded pulses stay.
async fn delete_strap(State(s): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<StatusCode> {
    if !blocking(&s.db, move |db| db.delete_strap(id)).await? {
        return Err(unknown_strap());
    }
    refresh_straps(&s).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Device, adapter, keys, server and database for the service page.
async fn service_status(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let service = s.service.clone();
    let (stats, size_bytes, adapter) = blocking(&s.db, move |db| {
        let size = |path: &FsPath| std::fs::metadata(path).map_or(0, |m| m.len());
        let mut wal = service.db_path.clone().into_os_string();
        wal.push("-wal");
        let size_bytes = size(&service.db_path) + size(FsPath::new(&wal));
        let adapter = service.serial_port.as_deref().and_then(adapter_info);
        Ok((db.stats()?, size_bytes, adapter))
    })
    .await?;
    let service = &s.service;
    Ok(Json(json!({
        "device": service.monitor.info(),
        "device_url": service.device_url,
        "adapter": adapter,
        "keys": service.keys,
        "modem": service.monitor.modem(),
        "bluetooth": s.rides.straps().availability(),
        "server": { "version": VERSION, "commit": commit(), "started_at_ms": service.started_at_ms },
        "db": {
            "path": service.db_path.display().to_string(),
            "size_bytes": size_bytes,
            "schema_version": stats.schema_version,
            "persons": stats.persons,
            "programs": stats.programs,
            "rides": stats.rides,
        },
    })))
}

async fn ws_service(State(s): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| ws_service_session(socket, s.service.monitor))
}

fn json_frame(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

/// The trace history first, then new entries and modem line changes. A client that
/// falls behind gets the history again.
async fn ws_service_session(mut socket: WebSocket, monitor: Monitor) {
    let mut traces = monitor.subscribe();
    let mut modem = monitor.watch_modem();
    let history = || json_frame(json!({ "type": "history", "entries": monitor.history() }));
    if socket.send(history()).await.is_err() {
        return;
    }
    modem.mark_changed();
    loop {
        let frame = tokio::select! {
            entry = traces.recv() => match entry {
                Ok(entry) => json_frame(json!({ "type": "trace", "entry": entry })),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Skip the backlog: the history already holds the newest entries.
                    traces = traces.resubscribe();
                    history()
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            changed = modem.changed() => {
                if changed.is_err() {
                    break;
                }
                let lines = *modem.borrow_and_update();
                json_frame(json!({ "type": "modem", "lines": lines }))
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        };
        if socket.send(frame).await.is_err() {
            break;
        }
    }
}

/// The whole database as a SQLite file; the browser picks the file name.
async fn download_backup(State(s): State<AppState>) -> ApiResult<Response> {
    let tmp = TempFile::new(&s.scratch_dir);
    let bytes = spawn_blocking(move || {
        s.db.backup_to(&tmp.0)?;
        Ok::<_, anyhow::Error>(std::fs::read(&tmp.0)?)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"kettler-backup.db\""),
        ],
        bytes,
    )
        .into_response())
}

async fn upload(dir: &FsPath, body: Bytes) -> ApiResult<TempFile> {
    let tmp = TempFile::new(dir);
    tokio::fs::write(&tmp.0, &body).await.map_err(ApiError::internal)?;
    Ok(tmp)
}

/// Checks an uploaded backup and reports what it holds; changes nothing.
async fn check_restore(State(s): State<AppState>, body: Bytes) -> ApiResult<Json<BackupInfo>> {
    let tmp = upload(&s.scratch_dir, body).await?;
    let info = spawn_blocking(move || inspect_backup(&tmp.0)).await.map_err(ApiError::internal)??;
    Ok(Json(info))
}

/// Replaces all data with the uploaded backup.
async fn restore(State(s): State<AppState>, body: Bytes) -> ApiResult<Json<BackupInfo>> {
    let tmp = upload(&s.scratch_dir, body).await?;
    // A task of its own owns the file: if the client goes away, the handler is dropped,
    // but the restore runs to its end before the file is removed.
    let rides = s.rides.clone();
    let task = tokio::spawn(async move {
        let result = rides.restore(tmp.0.clone()).await;
        drop(tmp);
        result
    });
    let info = task.await.map_err(ApiError::internal)??;
    // The restored data has its own straps.
    refresh_straps(&s).await?;
    Ok(Json(info))
}

/// Deletes the person with all rides and samples, unless one of their rides is running.
async fn delete_person(State(s): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<StatusCode> {
    s.rides.delete_person(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_ride(State(s): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<StatusCode> {
    s.rides.delete_ride(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_programs(State(s): State<AppState>) -> ApiResult<Json<Vec<Program>>> {
    Ok(Json(blocking(&s.db, |db| db.list_programs()).await?))
}

#[derive(Serialize)]
struct ProgramDetail {
    segments: Vec<FlatSegment>,
    total_secs: u32,
    program: Program,
    /// With `person_id`: the height the program gives that rider on target; else (or unknown) `null`.
    elevation_preview: Option<ElevationPreview>,
}

#[derive(Deserialize)]
struct ProgramQuery {
    person_id: Option<i64>,
}

async fn get_program(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiQuery(q): ApiQuery<ProgramQuery>,
) -> ApiResult<Json<ProgramDetail>> {
    let (program, person) = blocking(&s.db, move |db| {
        let person = match q.person_id {
            Some(pid) => db.list_persons()?.into_iter().find(|p| p.id == pid),
            None => None,
        };
        Ok((db.get_program(id)?, person))
    })
    .await?;
    let program = program.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Unbekanntes Programm".into()))?;
    let segments = program.expand();
    let elevation_preview = person.map(|p| {
        elevation::preview(&ElevationParams { weight_kg: p.weight_kg, flat_watts: p.flat_watts }, &segments)
    });
    Ok(Json(ProgramDetail { total_secs: program.total_secs(), segments, program, elevation_preview }))
}

async fn save_program(State(s): State<AppState>, ApiJson(program): ApiJson<Program>) -> ApiResult<Json<serde_json::Value>> {
    program
        .validate(&s.limits)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let id = blocking(&s.db, move |db| {
        if let Some(id) = program.id
            && db.get_program(id)?.is_none()
        {
            return Ok(None);
        }
        db.save_program(&program).map(Some)
    })
    .await?
    .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Unbekanntes Programm".into()))?;
    Ok(Json(json!({ "id": id })))
}

async fn delete_program(State(s): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<StatusCode> {
    blocking(&s.db, move |db| db.delete_program(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct StartRide {
    person_id: i64,
    #[serde(default)]
    program_id: Option<i64>,
}

async fn start_ride(State(s): State<AppState>, ApiJson(r): ApiJson<StartRide>) -> ApiResult<Json<serde_json::Value>> {
    let id = s.rides.start(r.person_id, r.program_id).await?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct RideQuery {
    person_id: i64,
}

async fn list_rides(State(s): State<AppState>, ApiQuery(q): ApiQuery<RideQuery>) -> ApiResult<Json<Vec<RideSummary>>> {
    Ok(Json(blocking(&s.db, move |db| db.list_rides(q.person_id)).await?))
}

async fn get_ride(State(s): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<RideDetail>> {
    blocking(&s.db, move |db| db.get_ride(id))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Unbekannte Fahrt".into()))
}

async fn limits(State(s): State<AppState>) -> Json<DeviceLimits> {
    Json(s.limits)
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({ "version": VERSION, "commit": commit() }))
}

async fn ws(State(s): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| ws_session(socket, s.rides))
}

#[derive(Serialize)]
struct LiveMessage<'a> {
    r#type: &'static str,
    #[serde(flatten)]
    view: &'a LiveView,
}

fn live_frame(view: &LiveView) -> Option<Message> {
    match serde_json::to_string(&LiveMessage { r#type: "live", view }) {
        Ok(text) => Some(Message::Text(text.into())),
        Err(e) => {
            tracing::error!("cannot serialize live view: {e}");
            None
        }
    }
}

async fn ws_session(mut socket: WebSocket, rides: RideManager) {
    let mut view = rides.watch();
    // The first frame is the current state, then one per change.
    view.mark_changed();
    loop {
        tokio::select! {
            changed = view.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(frame) = live_frame(&view.borrow_and_update()) else { continue };
                if socket.send(frame).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<UiCommand>(&text) {
                    Ok(cmd) => rides.command(cmd),
                    Err(e) => tracing::debug!("ignoring invalid websocket message: {e}"),
                },
                Some(Ok(Message::Binary(_))) => tracing::debug!("ignoring binary websocket frame"),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn static_file(method: Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let last = path.rsplit('/').next().unwrap_or("");
    let is_api = path == "api" || path.starts_with("api/");
    // Unit tests of the web modules live next to them but are not part of the app.
    if is_api || !matches!(method, Method::GET | Method::HEAD) || last.ends_with(".test.js") {
        return ApiError(StatusCode::NOT_FOUND, "Nicht gefunden".into()).into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    // SPA fallback: unknown extension-less paths get the index page.
    let (path, file) = match Assets::get(path) {
        Some(file) => (path, file),
        None if last.contains('.') => return ApiError(StatusCode::NOT_FOUND, "Nicht gefunden".into()).into_response(),
        None => match Assets::get("index.html") {
            Some(file) => ("index.html", file),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    // Revalidate on every load so an updated app reaches the iPad without clearing caches.
    ([(header::CONTENT_TYPE, content_type(path)), (header::CACHE_CONTROL, "no-cache")], file.data).into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;
    use crate::device::spawn_device;
    use crate::strap::{Availability, Found, Link, ScanError, StrapHub, TestStraps};
    use crate::transport::DeviceUrl;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use futures_util::{SinkExt, StreamExt};
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tower::ServiceExt;

    fn state() -> AppState {
        state_in(std::env::temp_dir())
    }

    fn state_in(scratch_dir: std::path::PathBuf) -> AppState {
        state_with_hub(scratch_dir, None)
    }

    /// With a test strap hub that the test drives.
    fn state_with_straps() -> (AppState, TestStraps) {
        let (hub, test) = StrapHub::test();
        (state_with_hub(std::env::temp_dir(), Some(hub)), test)
    }

    fn state_with_hub(scratch_dir: std::path::PathBuf, hub: Option<StrapHub>) -> AppState {
        let db = open_in_memory().unwrap();
        let device = spawn_device(DeviceUrl::Sim, Duration::from_millis(100));
        let service = ServiceInfo {
            monitor: device.monitor(),
            device_url: "sim://".into(),
            serial_port: None,
            keys: "off",
            db_path: scratch_dir.join("none.db"),
            started_at_ms: 1,
        };
        let hub = hub.unwrap_or_else(|| StrapHub::spawn(crate::strap::Mode::Off, device.monitor()));
        AppState {
            service,
            rides: RideManager::with_straps(device, db.clone(), true, hub),
            db,
            limits: DeviceLimits::default(),
            allowed_hosts: vec![],
            scratch_dir,
        }
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Option<serde_json::Value>) -> (StatusCode, String, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri).header(header::HOST, "localhost");
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, ctype, json)
    }

    #[tokio::test]
    async fn invalid_program_rejected_with_message() {
        let app = router(state());
        let (st, _, body) = call(&app, "POST", "/api/programs", Some(json!({"name": "X", "blocks": []}))).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains("Abschnitt") || msg.contains("Programm"), "{msg}");
        let (_, _, list) = call(&app, "GET", "/api/programs", None).await;
        // Only the templates, the invalid program was not saved.
        assert_eq!(list.as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn second_ride_start_conflicts() {
        let app = router(state());
        let (_, _, p) = call(&app, "POST", "/api/persons", Some(json!({"name": "Anna"}))).await;
        let body = json!({"person_id": p["id"]});
        let (st, _, r) = call(&app, "POST", "/api/rides", Some(body.clone())).await;
        assert_eq!(st, StatusCode::OK);
        assert!(r["id"].is_i64());
        let (st, _, r) = call(&app, "POST", "/api/rides", Some(body)).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(r["error"], "Es läuft bereits eine Fahrt");
    }

    #[tokio::test]
    async fn unknown_person_and_blank_name() {
        let app = router(state());
        let (st, _, _) = call(&app, "POST", "/api/rides", Some(json!({"person_id": 99}))).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _, r) = call(&app, "POST", "/api/persons", Some(json!({"name": "  "}))).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r["error"].is_string());
    }

    #[tokio::test]
    async fn serves_index_html() {
        let app = router(state());
        let (st, ctype, _) = call(&app, "GET", "/", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(ctype.starts_with("text/html"), "{ctype}");
        let (st, ctype, _) = call(&app, "GET", "/some/spa/route", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(ctype.starts_with("text/html"));
        let (st, _, r) = call(&app, "GET", "/api/nope", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert!(r["error"].is_string());
    }

    #[tokio::test]
    async fn static_files_are_revalidated_and_tests_hidden() {
        let app = router(state());
        for uri in ["/", "/app.js", "/style.css", "/some/spa/route"] {
            let req = Request::builder().uri(uri).header(header::HOST, "localhost").body(Body::empty()).unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert_eq!(resp.headers().get(header::CACHE_CONTROL).unwrap(), "no-cache", "{uri}");
        }
        let (st, _, r) = call(&app, "GET", "/lib/format.test.js", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert!(r["error"].is_string());
        let (st, ctype, _) = call(&app, "GET", "/lib/format.js", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(ctype.starts_with("text/javascript"));
    }

    #[tokio::test]
    async fn program_detail_includes_segments() {
        let st = state();
        let program = Program {
            id: None,
            name: "Intervalle".into(),
            blocks: vec![crate::program::Block {
                repeat: 6,
                steps: vec![
                    crate::program::Step { name: "hart".into(), duration_secs: 120, start_watts: 150, end_watts: 150 },
                    crate::program::Step { name: "leicht".into(), duration_secs: 230, start_watts: 80, end_watts: 80 },
                ],
            }],
        };
        let id = st.db.save_program(&program).unwrap();
        let app = router(st);
        let (code, _, body) = call(&app, "GET", &format!("/api/programs/{id}"), None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["segments"].as_array().unwrap().len(), 12);
        assert_eq!(body["total_secs"], 2100);
        assert_eq!(body["program"]["name"], "Intervalle");
        let (code, _, _) = call(&app, "DELETE", &format!("/api/programs/{id}"), None).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let (code, _, _) = call(&app, "GET", &format!("/api/programs/{id}"), None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    async fn call_with(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if !headers.iter().any(|(k, _)| *k == "host") {
            req = req.header(header::HOST, "localhost");
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    async fn person_with_rides(st: &AppState, name: &str, rides: u32) -> i64 {
        let id = st.db.add_person(name).unwrap();
        for _ in 0..rides {
            let ride = st.db.start_ride(id, None, 1000).unwrap().0;
            st.db
                .add_sample(
                    ride,
                    &crate::session::Sample {
                        ride_secs: 1,
                        status: crate::protocol::Status {
                            pulse: 0,
                            rpm: 80,
                            speed_x10: 100,
                            distance: 0,
                            target_watts: 50,
                            energy: 0,
                            elapsed_secs: 1,
                            actual_watts: 50,
                            recovery: false,
                        },
                        target_watts: 50,
                        at_unix_ms: None,
                        strap_pulse: None,
                    },
                )
                .unwrap();
            st.db.finish_ride(ride, &crate::db::RideSummary {
                id: ride, person_id: id, program_name: None, started_at: 1000, ride_secs: 1,
                avg_watts: 50, max_watts: 50, avg_pulse: None, max_pulse: None,
                distance_m: 0.0, energy_kj: 0.0, finished: true, ended_at: Some(1001),
                avg_rpm: Some(80), max_rpm: Some(80),
                weight_kg: 75.0, flat_watts: 70, elevation_gain_m: 0.0,
            })
            .unwrap();
        }
        id
    }

    #[tokio::test]
    async fn delete_ride_removes_it_with_samples() {
        let st = state();
        let person = person_with_rides(&st, "Anna", 2).await;
        let rides = st.db.list_rides(person).unwrap();
        let app = router(st.clone());
        let uri = format!("/api/rides/{}", rides[0].id);
        let (code, _, _) = call(&app, "DELETE", &uri, None).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let (code, _, _) = call(&app, "GET", &uri, None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _, r) = call(&app, "DELETE", &uri, None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(r["error"], "Unbekannte Fahrt");
        assert_eq!(st.db.list_rides(person).unwrap().len(), 1);
        assert_eq!(st.db.get_ride(rides[1].id).unwrap().unwrap().samples.len(), 1);
    }

    #[tokio::test]
    async fn malformed_delete_ids_are_400() {
        let app = router(state());
        for uri in ["/api/persons/abc", "/api/rides/abc", "/api/persons/abc/stats"] {
            let method = if uri.ends_with("stats") { "GET" } else { "DELETE" };
            let (st, _, r) = call(&app, method, uri, None).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(r["error"], "Ungültige Anfrage");
        }
    }

    #[tokio::test]
    async fn person_stats_and_delete_with_rides() {
        let st = state();
        let alice = person_with_rides(&st, "Alice", 3).await;
        let bob = person_with_rides(&st, "Bob", 1).await;
        let app = router(st.clone());
        let (code, _, r) = call(&app, "GET", &format!("/api/persons/{alice}/stats"), None).await;
        assert_eq!((code, r["rides"].as_i64()), (StatusCode::OK, Some(3)));
        let (code, _, _) = call(&app, "GET", "/api/persons/99/stats", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _, _) = call(&app, "DELETE", &format!("/api/persons/{alice}"), None).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let (code, _, r) = call(&app, "DELETE", &format!("/api/persons/{alice}"), None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(r["error"], "Unbekannte Person");
        assert_eq!(st.db.list_persons().unwrap().len(), 1);
        assert_eq!(st.db.count_rides(alice).unwrap(), 0);
        assert_eq!(st.db.count_rides(bob).unwrap(), 1);
    }

    #[tokio::test]
    async fn running_ride_blocks_deleting_it_and_its_person() {
        let st = state();
        let anna = person_with_rides(&st, "Anna", 1).await;
        let bob = person_with_rides(&st, "Bob", 1).await;
        let ride = st.rides.start(anna, None).await.unwrap();
        let app = router(st.clone());
        // The view is published right after the start.
        let mut rx = st.rides.watch();
        while rx.borrow_and_update().ride.is_none() {
            rx.changed().await.unwrap();
        }
        let (code, _, r) = call(&app, "DELETE", &format!("/api/rides/{ride}"), None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(r["error"], "Fahrt läuft noch – zuerst beenden");
        let (code, _, r) = call(&app, "DELETE", &format!("/api/persons/{anna}"), None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(r["error"], "Für diese Person läuft gerade eine Fahrt");
        assert_eq!(st.db.list_persons().unwrap().len(), 2);
        // Other persons and finished rides of the same person are not blocked.
        let (code, _, _) = call(&app, "DELETE", &format!("/api/persons/{bob}"), None).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        let old = st.db.list_rides(anna).unwrap().into_iter().find(|r| r.id != ride).unwrap();
        let (code, _, _) = call(&app, "DELETE", &format!("/api/rides/{}", old.id), None).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn saving_unknown_program_id_is_404() {
        let app = router(state());
        let body = json!({"id": 99, "name": "X", "blocks": [{"repeat": 1, "steps": [{"name": "a", "duration_secs": 60, "start_watts": 100, "end_watts": 100}]}]});
        let (st, _, r) = call(&app, "POST", "/api/programs", Some(body)).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(r["error"], "Unbekanntes Programm");
    }

    #[tokio::test]
    async fn rename_person_api() {
        let st = state();
        let id = st.db.add_person("Bob").unwrap();
        let app = router(st.clone());
        let uri = format!("/api/persons/{id}");
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"name": " Alice "}))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!((&r["id"], &r["name"]), (&json!(id), &json!("Alice")));
        assert_eq!(st.db.list_persons().unwrap()[0].name, "Alice");
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"name": "  "}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Name darf nicht leer sein");
        let (code, _, r) = call(&app, "PUT", "/api/persons/99", Some(json!({"name": "X"}))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(r["error"], "Unbekannte Person");
        let foreign = [("host", "localhost:8080"), ("origin", "http://evil.example")];
        let (code, _) = call_with(&app, "PUT", &uri, &foreign, Some(json!({"name": "Eve"}))).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(st.db.list_persons().unwrap()[0].name, "Alice");
    }

    #[tokio::test]
    async fn put_person_profile() {
        let st = state();
        let id = st.db.add_person("Bob").unwrap();
        let app = router(st.clone());
        let uri = format!("/api/persons/{id}");
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"weight_kg": 85, "flat_watts": 80}))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(r, json!({"id": id, "name": "Bob", "weight_kg": 85.0, "flat_watts": 80, "strap_id": null}));
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"weight_kg": 20}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Gewicht muss zwischen 30 und 250 kg liegen");
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Keine Änderung angegeben");
        let (code, _, r) = call(&app, "PUT", "/api/persons/99", Some(json!({"flat_watts": 60}))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(r["error"], "Unbekannte Person");
        let (_, _, list) = call(&app, "GET", "/api/persons", None).await;
        assert_eq!(list, json!([{"id": id, "name": "Bob", "weight_kg": 85.0, "flat_watts": 80, "strap_id": null}]));
    }

    #[tokio::test]
    async fn put_person_name_only_keeps_profile() {
        let st = state();
        let id = st.db.add_person("Bob").unwrap();
        let app = router(st.clone());
        let uri = format!("/api/persons/{id}");
        let (code, _, _) = call(&app, "PUT", &uri, Some(json!({"weight_kg": 85, "flat_watts": 80}))).await;
        assert_eq!(code, StatusCode::OK);
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"name": "Alice"}))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(r, json!({"id": id, "name": "Alice", "weight_kg": 85.0, "flat_watts": 80, "strap_id": null}));
    }

    #[tokio::test]
    async fn foreign_origin_rejected() {
        let app = router(state());
        let foreign = [("host", "localhost:8080"), ("origin", "http://evil.example")];
        let (st, r) = call_with(&app, "POST", "/api/persons", &foreign, Some(json!({"name": "A"}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(r["error"], "Fremde Herkunft abgelehnt");
        let (st, _) = call_with(&app, "POST", "/api/rides", &foreign, Some(json!({"person_id": 1}))).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = call_with(&app, "DELETE", "/api/programs/1", &foreign, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        for uri in ["/api/rides/1", "/api/persons/1"] {
            let (st, _) = call_with(&app, "DELETE", uri, &foreign, None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}");
            let (st, _) = call_with(&app, "DELETE", uri, &[("host", "evil.example.com")], None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}");
        }
        let upgrade = [
            ("host", "localhost:8080"),
            ("origin", "http://evil.example"),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ];
        let (st, _) = call_with(&app, "GET", "/ws", &upgrade, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // Same origin passes; GETs are not guarded.
        let same = [("host", "localhost:8080"), ("origin", "http://localhost:8080")];
        let (st, _) = call_with(&app, "POST", "/api/persons", &same, Some(json!({"name": "A"}))).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call_with(&app, "GET", "/api/persons", &foreign, None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn host_allowlist() {
        let mut st = state();
        st.allowed_hosts = vec!["kettler.example.org".into()];
        let app = router(st);
        let post = |host: &'static str| {
            let app = app.clone();
            async move {
                let origin = format!("http://{host}");
                call_with(&app, "POST", "/api/persons", &[("host", host), ("origin", &origin)], Some(json!({"name": "A"}))).await
            }
        };
        let (st, r) = post("evil.example.com:8080").await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(r["error"], "Unbekannter Hostname");
        for ok in ["192.168.1.20:8080", "nas:8080", "NAS", "kettler.fritz.box:8080", "Foo.LOCAL", "[::1]:8080", "localhost:8080", "kettler.example.org:8080"] {
            assert_eq!(post(ok).await.0, StatusCode::OK, "{ok}");
        }
        let upgrade = [
            ("host", "evil.example.com:8080"),
            ("origin", "http://evil.example.com:8080"),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ];
        let (st, r) = call_with(&app, "GET", "/ws", &upgrade, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(r["error"], "Unbekannter Hostname");
        // No Host at all (and no authority) is rejected; with an absolute-form URI the authority counts.
        let req = Request::builder().method("POST").uri("/api/persons").header(header::CONTENT_TYPE, "application/json");
        let resp = app.clone().oneshot(req.body(Body::from(r#"{"name":"A"}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let req = Request::builder().method("POST").uri("http://nas:8080/api/persons").header(header::CONTENT_TYPE, "application/json");
        let resp = app.clone().oneshot(req.body(Body::from(r#"{"name":"A"}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // API GETs are guarded by the host check too; static files are not.
        for uri in ["/api/persons", "/api/rides/1", "/api/limits", "/api/version", "/api"] {
            let (st, r) = call_with(&app, "GET", uri, &[("host", "evil.example.com")], None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(r["error"], "Unbekannter Hostname");
        }
        let (st, _) = call_with(&app, "GET", "/api/persons", &[("host", "nas:8080")], None).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call_with(&app, "HEAD", "/", &[("host", "evil.example.com")], None).await;
        assert_eq!(st, StatusCode::OK);
    }

    #[tokio::test]
    async fn run_stops_on_shutdown_signal() {
        let (trigger, signal) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(run("127.0.0.1:0".parse().unwrap(), state(), async move {
            let _ = signal.await;
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!server.is_finished());
        trigger.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), server).await.expect("server did not stop");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn extractor_rejections_are_json() {
        let app = router(state());
        let (st, r) = call_with(&app, "POST", "/api/persons", &[], Some(json!({"name": 1}))).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Ungültige Anfrage");
        let (st, r) = call_with(&app, "GET", "/api/rides", &[], None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(r["error"], "Ungültige Anfrage");
        let (st, r) = call_with(&app, "GET", "/api/programs/abc", &[], None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(r["error"], "Ungültige Anfrage");
    }

    #[tokio::test]
    async fn spa_fallback_only_for_get_without_extension() {
        let app = router(state());
        let (st, _, _) = call(&app, "GET", "/missing.js", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _, _) = call(&app, "POST", "/anything", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn limits_and_ride_lookup() {
        let app = router(state());
        let (_, _, l) = call(&app, "GET", "/api/limits", None).await;
        assert_eq!(l, json!({"min": 25, "max": 400, "step": 5}));
        let (st, _, _) = call(&app, "GET", "/api/rides/42", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _, l) = call(&app, "GET", "/api/rides?person_id=1", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(l, json!([]));
    }

    #[tokio::test]
    async fn version_endpoint_reports_version_and_commit() {
        let app = router(state());
        let (st, _, v) = call(&app, "GET", "/api/version", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["commit"], commit());
        assert!(!commit().is_empty());
    }

    #[tokio::test]
    async fn ws_receives_live_updates() {
        use tokio_tungstenite::tungstenite::Message as M;
        let st = state();
        let (rides, db) = (st.rides.clone(), st.db.clone());
        let app = router(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
        assert!(first.into_text().unwrap().contains("\"type\":\"live\""));

        ws.send(M::text(r#"{"type":"adjust","delta":10}"#)).await.unwrap();
        ws.send(M::text("xyz")).await.unwrap();
        // Still open: a ping gets answered.
        ws.send(M::Ping(vec![1].into())).await.unwrap();
        let mut got_pong = false;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(m))) if m.is_pong() => {
                    got_pong = true;
                    break;
                }
                Ok(Some(Ok(_))) => {}
                other => panic!("connection closed: {other:?}"),
            }
        }
        assert!(got_pong);

        // A started ride shows up as a live frame with `ride` set, and `adjust` changes its target.
        let person = db.add_person("Anna").unwrap();
        rides.start(person, None).await.unwrap();
        let target = |v: &serde_json::Value| v["ride"]["session"]["target_watts"].as_u64();
        let mut first_target = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while first_target.is_none() {
            let m = tokio::time::timeout_at(deadline, ws.next()).await.expect("no ride frame").unwrap().unwrap();
            if let Ok(text) = m.into_text() {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["type"], "live");
                // `adjust` is only honored once the session is active.
                if v["ride"]["session"]["phase"] == "Active" {
                    first_target = target(&v);
                }
            }
        }
        ws.send(M::text(r#"{"type":"adjust","delta":10}"#)).await.unwrap();
        let mut changed = false;
        while !changed {
            let m = tokio::time::timeout_at(deadline, ws.next()).await.expect("target never changed").unwrap().unwrap();
            if let Ok(text) = m.into_text() {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                changed = target(&v).is_some_and(|t| Some(t) != first_target);
            }
        }
    }

    async fn call_bytes(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(uri).header(header::HOST, "localhost");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if !body.is_empty() {
            req = req.header(header::CONTENT_TYPE, "application/octet-stream");
        }
        let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        (status, headers, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    fn json_of(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn backup_download_is_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        let (st, headers, body) = call_bytes(&app, "GET", "/api/service/backup", &[], vec![]).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.starts_with(b"SQLite format 3\0"));
        assert!(headers[header::CONTENT_DISPOSITION].to_str().unwrap().contains("attachment"));
    }

    #[tokio::test]
    async fn restore_check_counts_and_restore_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        call(&app, "POST", "/api/persons", Some(json!({"name": "Bob"}))).await;
        let (_, _, backup) = call_bytes(&app, "GET", "/api/service/backup", &[], vec![]).await;
        call(&app, "POST", "/api/persons", Some(json!({"name": "Alice"}))).await;
        let (st, _, info) = call_bytes(&app, "POST", "/api/service/restore/check", &[], backup.clone()).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(json_of(&info)["persons"], 1);
        let (_, _, persons) = call(&app, "GET", "/api/persons", None).await;
        assert_eq!(persons.as_array().unwrap().len(), 2, "check must not change anything");
        let (st, _, info) = call_bytes(&app, "POST", "/api/service/restore", &[], backup).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(json_of(&info)["persons"], 1);
        let (_, _, persons) = call(&app, "GET", "/api/persons", None).await;
        let names: Vec<_> = persons.as_array().unwrap().iter().map(|p| p["name"].clone()).collect();
        assert_eq!(names, [json!("Bob")]);
    }

    #[tokio::test]
    async fn restore_garbage_is_422() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        for uri in ["/api/service/restore/check", "/api/service/restore"] {
            let (st, _, body) = call_bytes(&app, "POST", uri, &[], b"hello".to_vec()).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
            assert_eq!(json_of(&body)["error"], "Keine gültige Kettler-Datenbank");
        }
    }

    #[tokio::test]
    async fn restore_during_ride_is_409() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        let (_, _, p) = call(&app, "POST", "/api/persons", Some(json!({"name": "Bob"}))).await;
        let (_, _, backup) = call_bytes(&app, "GET", "/api/service/backup", &[], vec![]).await;
        let (st, _, _) = call(&app, "POST", "/api/rides", Some(json!({"person_id": p["id"]}))).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _, body) = call_bytes(&app, "POST", "/api/service/restore", &[], backup).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(json_of(&body)["error"], "Während einer Fahrt nicht möglich");
    }

    #[tokio::test]
    async fn restore_from_foreign_origin_is_403() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        let (_, _, backup) = call_bytes(&app, "GET", "/api/service/backup", &[], vec![]).await;
        let (st, _, _) =
            call_bytes(&app, "POST", "/api/service/restore", &[("origin", "http://evil.example")], backup).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn restore_accepts_large_body() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        let (st, _, _) = call_bytes(&app, "POST", "/api/service/restore/check", &[], vec![b'x'; 3 * 1024 * 1024]).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn scratch_dir_is_left_clean() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(state_in(dir.path().into()));
        let (_, _, backup) = call_bytes(&app, "GET", "/api/service/backup", &[], vec![]).await;
        call_bytes(&app, "POST", "/api/service/restore/check", &[], backup.clone()).await;
        call_bytes(&app, "POST", "/api/service/restore", &[], backup).await;
        call_bytes(&app, "POST", "/api/service/restore", &[], b"hello".to_vec()).await;
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test]
    async fn program_detail_has_the_riders_elevation_preview() {
        let st = state();
        let light = st.db.add_person("Alice").unwrap();
        let heavy = st.db.add_person("Bob").unwrap();
        let weight = |kg: f64| PersonUpdate { weight_kg: Some(kg), ..PersonUpdate::default() };
        st.db.update_person(light, &weight(60.0)).unwrap();
        st.db.update_person(heavy, &weight(100.0)).unwrap();
        let program = st.db.list_programs().unwrap().into_iter().find(|p| p.name == "Pyramide").unwrap().id.unwrap();
        let app = router(st);
        let gain = |body: &serde_json::Value| body["elevation_preview"]["gain_m"].as_f64().unwrap();

        let (_, _, plain) = call(&app, "GET", &format!("/api/programs/{program}"), None).await;
        assert!(plain["elevation_preview"].is_null(), "no rider, no height");
        let (code, _, a) = call(&app, "GET", &format!("/api/programs/{program}?person_id={light}"), None).await;
        assert_eq!(code, StatusCode::OK);
        let (_, _, b) = call(&app, "GET", &format!("/api/programs/{program}?person_id={heavy}"), None).await;
        assert!(gain(&a) > gain(&b), "the lighter rider climbs more: {} vs {}", gain(&a), gain(&b));
        assert!(a["elevation_preview"]["points"].as_array().unwrap().len() > 2);
        let (code, _, unknown) = call(&app, "GET", &format!("/api/programs/{program}?person_id=999"), None).await;
        assert_eq!(code, StatusCode::OK);
        assert!(unknown["elevation_preview"].is_null());
    }

    #[tokio::test]
    async fn strap_crud_endpoints() {
        let (st, test) = state_with_straps();
        let app = router(st.clone());
        let new = json!({"address": "aa:bb:cc:dd:ee:ff", "name": "HRM-Dual"});
        let (code, _, r) = call(&app, "POST", "/api/straps", Some(new.clone())).await;
        assert_eq!(code, StatusCode::OK);
        let id = r["id"].as_i64().unwrap();
        assert_eq!(test.known().iter().map(|s| s.address.as_str()).collect::<Vec<_>>(), ["AA:BB:CC:DD:EE:FF"]);
        let (code, _, r) = call(&app, "POST", "/api/straps", Some(new)).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Dieser Gurt ist schon angemeldet");
        let (code, _, _) = call(&app, "POST", "/api/straps", Some(json!({"address": "11:22:33:44:55:66", "name": " "}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

        test.set(id, Link::Connected, Some(72), Duration::ZERO);
        let uri = format!("/api/straps/{id}");
        let (code, _, _) = call(&app, "PUT", &uri, Some(json!({"name": "Bobs Gurt"}))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(test.known()[0].name, "Bobs Gurt");
        let (code, _, r) = call(&app, "GET", "/api/straps", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(r["availability"], json!({"state": "ready"}));
        let listed = &r["straps"][0];
        assert_eq!((&listed["name"], &listed["link"], &listed["pulse"]), (&json!("Bobs Gurt"), &json!("connected"), &json!(72)));
        let (code, _, _) = call(&app, "PUT", "/api/straps/999", Some(json!({"name": "x"}))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let (code, _, _) = call(&app, "DELETE", &uri, None).await;
        assert_eq!(code, StatusCode::NO_CONTENT);
        assert!(test.known().is_empty());
        let (code, _, _) = call(&app, "DELETE", &uri, None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn straps_without_backend_state_are_out_of_range() {
        let st = state();
        st.db.add_strap("AA:BB:CC:DD:EE:FF", "HRM-Dual").unwrap();
        let (_, _, r) = call(&router(st), "GET", "/api/straps", None).await;
        assert_eq!(r["availability"], json!({"state": "off"}));
        assert_eq!(r["straps"][0]["link"], "out_of_range");
        assert_eq!(r["straps"][0]["pulse"], json!(null));
    }

    #[tokio::test]
    async fn scan_maps_errors() {
        let (st, test) = state_with_straps();
        st.db.add_strap("AA:BB:CC:DD:EE:FF", "HRM-Dual").unwrap();
        let app = router(st);
        let found = |address: &str| Found { address: address.into(), name: Some("HRM-Dual".into()), rssi: Some(-60) };
        test.set_scan(Ok(vec![found("AA:BB:CC:DD:EE:FF"), found("11:22:33:44:55:66")]), Duration::ZERO);
        let (code, _, r) = call(&app, "POST", "/api/straps/scan", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!((&r[0]["known"], &r[1]["known"]), (&json!(true), &json!(false)));
        assert_eq!(r[1]["rssi"], -60);
        for (err, status) in [
            (ScanError::Busy, StatusCode::CONFLICT),
            (ScanError::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
            (ScanError::Failed("x".into()), StatusCode::BAD_GATEWAY),
        ] {
            test.set_scan(Err(err.clone()), Duration::ZERO);
            let (code, _, r) = call(&app, "POST", "/api/straps/scan", None).await;
            assert_eq!(code, status);
            assert_eq!(r["error"], err.to_string());
        }
        let off = router(state());
        let (code, _, _) = call(&off, "POST", "/api/straps/scan", None).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn person_strap_assignment() {
        let st = state();
        let id = st.db.add_person("Bob").unwrap();
        let strap = st.db.add_strap("AA:BB:CC:DD:EE:FF", "HRM-Dual").unwrap();
        let app = router(st.clone());
        let uri = format!("/api/persons/{id}");
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"strap_id": strap}))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(r["strap_id"], strap);
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"name": "Robert"}))).await;
        assert_eq!((code, &r["strap_id"]), (StatusCode::OK, &json!(strap)));
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"strap_id": null}))).await;
        assert_eq!((code, &r["strap_id"]), (StatusCode::OK, &json!(null)));
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({"strap_id": 999}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Unbekannter Gurt");
        let (code, _, r) = call(&app, "PUT", &uri, Some(json!({}))).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r["error"], "Keine Änderung angegeben");
    }

    #[tokio::test]
    async fn service_status_has_bluetooth() {
        let (st, test) = state_with_straps();
        test.set_availability(Availability::Unavailable { reason: "kein Adapter".into() });
        let (_, _, r) = call(&router(st), "GET", "/api/service/status", None).await;
        assert_eq!(r["bluetooth"], json!({"state": "unavailable", "reason": "kein Adapter"}));
    }

    #[tokio::test]
    async fn service_status_reports_server_and_db() {
        let app = router(state());
        let (st, _, body) = call(&app, "GET", "/api/service/status", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["server"]["version"], VERSION);
        assert_eq!(body["server"]["started_at_ms"], 1);
        assert_eq!(body["db"]["schema_version"], 6);
        assert_eq!(body["db"]["programs"], 4);
        assert_eq!(body["db"]["size_bytes"], 0);
        assert_eq!(body["keys"], "off");
        assert_eq!(body["device_url"], "sim://");
        assert!(body["adapter"].is_null());
        assert!(body["modem"].is_null());
        assert!(body["device"]["connected"].is_boolean());
    }

    async fn next_json<S>(ws: &mut S) -> serde_json::Value
    where
        S: futures_util::Stream<Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
        serde_json::from_str(&msg.into_text().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn service_ws_sends_history_then_traces() {
        use crate::monitor::TraceKind;
        let st = state();
        let monitor = st.service.monitor.clone();
        monitor.trace(TraceKind::Sent, "marker-old");
        let app = router(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/service")).await.unwrap();
        let first = next_json(&mut ws).await;
        assert_eq!(first["type"], "history");
        assert!(first["entries"].as_array().unwrap().iter().any(|e| e["text"] == "marker-old"), "{first}");
        monitor.trace(TraceKind::Received, "marker-new");
        loop {
            let frame = next_json(&mut ws).await;
            if frame["type"] == "trace" && frame["entry"]["text"] == "marker-new" {
                assert_eq!(frame["entry"]["kind"], "received");
                break;
            }
        }
    }

    #[tokio::test]
    async fn service_ws_sends_modem_and_recovers_from_lag() {
        use crate::monitor::TraceKind;
        let st = state();
        let monitor = st.service.monitor.clone();
        let app = router(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/service")).await.unwrap();
        assert_eq!(next_json(&mut ws).await["type"], "history");
        let modem = next_json(&mut ws).await;
        assert_eq!(modem["type"], "modem");
        assert!(modem["lines"].is_null());

        // Far more than the broadcast holds; the single-threaded test runtime lets the
        // session forward nothing meanwhile, so it lags.
        for i in 0..2000 {
            monitor.trace(TraceKind::Sent, format!("flood-{i}"));
        }
        loop {
            if next_json(&mut ws).await["type"] == "history" {
                break;
            }
        }
        // After the fresh history only new entries follow, no replay of old ones
        // (the simulator adds its own traffic in between).
        monitor.trace(TraceKind::Received, "after-history");
        loop {
            let frame = next_json(&mut ws).await;
            let text = frame["entry"]["text"].as_str().unwrap_or_default();
            assert!(!text.starts_with("flood-"), "{frame}");
            if text == "after-history" {
                break;
            }
        }
    }

    #[tokio::test]
    async fn service_ws_rejects_foreign_origin() {
        let app = router(state());
        let upgrade = [
            ("host", "localhost:8080"),
            ("origin", "http://evil.example"),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ];
        let (st, _) = call_with(&app, "GET", "/ws/service", &upgrade, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
}
