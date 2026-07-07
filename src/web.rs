//! HTTP API and WebApp server: axum routes, handlers, and serve state.

use std::collections::BTreeMap;
use std::fs::{self};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::*;

#[derive(Clone)]
pub(crate) struct WebState {
    auth_token: String,
    profiles: Arc<BTreeMap<String, WebProfile>>,
    max_upload_bytes: usize,
    max_object_upload_bytes: usize,
    cors_allow_origins: Vec<HeaderValue>,
}

#[derive(Clone)]
pub(crate) struct WebProfile {
    name: String,
    store: PathBuf,
    workspace: PathBuf,
    store_handle: Arc<Store>,
    mutation_lock: Arc<Mutex<()>>,
}

impl WebState {
    /// Opens every profile's store once and shares the handle across
    /// requests. RocksDB allows only a single open per store directory, so
    /// per-request opens would make concurrent API calls race on the LOCK
    /// file.
    pub(crate) fn try_new(config: ServeConfig) -> Result<Self> {
        let mut profiles = BTreeMap::new();
        for (name, profile) in config.profiles {
            let store_handle = Arc::new(Store::open(&profile.store)?);
            profiles.insert(
                name,
                WebProfile {
                    name: profile.name,
                    store: profile.store,
                    workspace: profile.workspace,
                    store_handle,
                    mutation_lock: Arc::new(Mutex::new(())),
                },
            );
        }
        Ok(Self {
            auth_token: config.auth_token,
            profiles: Arc::new(profiles),
            max_upload_bytes: config.max_upload_bytes,
            max_object_upload_bytes: config.max_object_upload_bytes,
            cors_allow_origins: config.cors_allow_origins,
        })
    }
}

impl ApiError {
    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

/// 500 whose detail only reaches the server log: these failures embed server
/// filesystem paths that must not leak into response bodies.
pub(crate) fn internal_error(detail: impl std::fmt::Display) -> ApiError {
    eprintln!("internal error: {detail}");
    ApiError::internal("internal server error")
}

/// Keeps extractor failures (oversized body, malformed JSON) in the same
/// `{"error": ...}` shape as every other API error instead of axum's
/// plain-text default, preserving the rejection's status (413 vs 400).
pub(crate) fn json_rejection_error(rejection: JsonRejection) -> ApiError {
    ApiError {
        status: rejection.status(),
        message: rejection.body_text(),
    }
}

pub(crate) async fn serve_web(config: ServeConfig) -> Result<()> {
    let bind = config.bind;
    let state = Arc::new(WebState::try_new(config)?);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    let local_addr = listener.local_addr().unwrap_or(bind);
    println!("serving asset browser at http://{local_addr}");
    axum::serve(listener, web_app(state))
        .await
        .context("asset browser server failed")
}

pub(crate) fn web_app(state: Arc<WebState>) -> Router {
    let max_upload_bytes = state.max_upload_bytes;
    let api = Router::new()
        .route("/profiles", get(api_profiles))
        .route("/assets", get(api_assets))
        .route("/asset", get(api_asset))
        .route("/versions", get(api_versions))
        // Version and wip imports carry a full manifest whose JSON grows
        // with file count; axum's default 2 MiB extractor limit rejects
        // ~10k-entry manifests these endpoints exist to accept, so they
        // share the object-upload cap instead.
        .route(
            "/version",
            get(api_version_info)
                .put(api_import_version)
                .layer(DefaultBodyLimit::max(state.max_object_upload_bytes)),
        )
        .route("/object/status", get(api_object_status))
        // No DefaultBodyLimit here: PUT enforces its cap while streaming to
        // disk, and the limit layer only guards buffering extractors anyway.
        .route("/object", get(api_object).put(api_upload_object))
        .route("/current/status", get(api_current_status))
        .route("/current", put(api_update_current))
        .route("/pull", post(api_pull))
        .route("/restore", post(api_restore))
        .route("/materialize", post(api_materialize))
        .route(
            "/thumbnails",
            post(api_upload_thumbnail).layer(DefaultBodyLimit::max(max_upload_bytes)),
        )
        .route("/thumbnail", put(api_import_thumbnail))
        .route("/thumbnail-url", get(api_thumbnail_url))
        .route("/resolve", get(api_resolve))
        .route("/wips", get(api_wips))
        .route(
            "/wip",
            post(api_wip).layer(DefaultBodyLimit::max(state.max_object_upload_bytes)),
        )
        .route("/promote", post(api_promote))
        .route("/gc", post(api_gc))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api_auth_middleware,
        ));

    let mut app = Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .nest("/api", api)
        .nest(
            "/objects",
            Router::new()
                .route("/sha256/{prefix}/{sha256}", get(object_by_path_default))
                .route(
                    "/{profile}/sha256/{prefix}/{sha256}",
                    get(object_by_path_profile),
                )
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    api_auth_middleware,
                )),
        )
        .with_state(state.clone());
    // CORS is opt-in per origin (`serve --cors-allow-origin`): the bundled
    // WebApp is served same-origin, and the CLI and resolver are not
    // browsers, so by default no CORS headers are sent at all.
    if !state.cors_allow_origins.is_empty() {
        app = app.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(state.cors_allow_origins.iter().cloned()))
                .allow_methods([Method::GET, Method::POST, Method::PUT])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    }
    app
}

pub(crate) async fn api_auth_middleware(
    State(state): State<Arc<WebState>>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, ApiError> {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let authorized =
        token.is_some_and(|token| constant_time_eq(token.as_bytes(), state.auth_token.as_bytes()));
    if authorized {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::unauthorized("missing or invalid bearer token"))
    }
}

/// Constant-time byte comparison for the bearer-token check, so response
/// timing does not leak how much of a guessed token matched. The length
/// itself is not treated as secret.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

pub(crate) async fn index_html() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub(crate) async fn app_js() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
        .into_response()
}

pub(crate) async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

pub(crate) async fn api_profiles(
    State(state): State<Arc<WebState>>,
) -> std::result::Result<Json<ProfilesResponse>, ApiError> {
    let profiles = state
        .profiles
        .values()
        .map(|profile| ProfileDto {
            name: profile.name.clone(),
            store: profile.store.display().to_string(),
            workspace: profile.workspace.display().to_string(),
        })
        .collect();
    Ok(Json(ProfilesResponse { profiles }))
}

pub(crate) async fn api_assets(
    State(state): State<Arc<WebState>>,
    Query(query): Query<AssetsQuery>,
) -> std::result::Result<Json<AssetsResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = profile.store_handle.clone();
        build_asset_cards(&store, &query)
    })
    .await
    .map(Json)
}

pub(crate) async fn api_asset(
    State(state): State<Arc<WebState>>,
    Query(query): Query<AssetQuery>,
) -> std::result::Result<Json<AssetDetailResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(query.category, query.asset_code)?;
        let info = store.asset_info(&asset_key)?;
        let current_status =
            store.current_status(Some(&asset_key.category), Some(&asset_key.asset_code), None)?;
        let thumbnails =
            store.list_thumbnails(Some(&asset_key.category), Some(&asset_key.asset_code), None)?;
        Ok(AssetDetailResponse {
            info,
            current_status,
            thumbnails,
        })
    })
    .await
    .map(Json)
}

pub(crate) async fn api_versions(
    State(state): State<Arc<WebState>>,
    Query(query): Query<VersionsQuery>,
) -> std::result::Result<Json<VersionsResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(query.category, query.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, query.department)?;
        let versions = store.list_versions(
            Some(&department_key.asset_key.category),
            Some(&department_key.asset_key.asset_code),
            Some(&department_key.department),
        )?;
        let current_status = store.current_status_for_department(&department_key)?;
        let thumbnails = store.list_thumbnails(
            Some(&department_key.asset_key.category),
            Some(&department_key.asset_key.asset_code),
            Some(&department_key.department),
        )?;
        Ok(VersionsResponse {
            versions,
            current_status,
            thumbnails,
        })
    })
    .await
    .map(Json)
}

pub(crate) async fn api_version_info(
    State(state): State<Arc<WebState>>,
    Query(query): Query<VersionInfoQuery>,
) -> std::result::Result<Json<VersionInfo>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(query.category, query.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, query.department)?;
        let selector = if query.latest.unwrap_or(false) {
            VersionSelector::Latest
        } else {
            query
                .version
                .map_or(VersionSelector::Current, VersionSelector::Version)
        };
        store.version_info_by_selector(&department_key, selector)
    })
    .await
    .map(Json)
}

pub(crate) async fn api_import_version(
    State(state): State<Arc<WebState>>,
    payload: std::result::Result<Json<VersionImportRequest>, JsonRejection>,
) -> std::result::Result<Json<VersionRecord>, ApiError> {
    let Json(request) = payload.map_err(json_rejection_error)?;
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        for entry in &request.version_info.manifest.entries {
            if !store.object_is_valid(&entry.sha256, entry.size)? {
                bail!(
                    "object missing or invalid for {}: {}",
                    entry.relative_path,
                    entry.sha256
                );
            }
        }
        store.import_version_info(&request.version_info)?;
        Ok(request.version_info.version)
    })
    .await
    .map(Json)
}

pub(crate) async fn api_object_status(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ObjectStatusQuery>,
) -> std::result::Result<Json<ObjectStatusResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        validate_sha256(&query.sha256)?;
        let exists = if let Some(size) = query.size {
            let store = profile.store_handle.clone();
            store.object_is_valid(&query.sha256, size)?
        } else {
            object_path(&profile.store, &query.sha256).exists()
        };
        Ok(ObjectStatusResponse {
            sha256: query.sha256,
            exists,
        })
    })
    .await
    .map(Json)
}

pub(crate) async fn api_object(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ObjectQuery>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    object_response(profile, query.sha256, None, headers).await
}

pub(crate) async fn object_by_path_default(
    State(state): State<Arc<WebState>>,
    AxumPath((prefix, sha256)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let profile = profile_for_object_path(&state, None)?;
    object_response(profile, sha256, Some(prefix), headers).await
}

pub(crate) async fn object_by_path_profile(
    State(state): State<Arc<WebState>>,
    AxumPath((profile_name, prefix, sha256)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let profile = profile_for_object_path(&state, Some(&profile_name))?;
    object_response(profile, sha256, Some(prefix), headers).await
}

async fn object_response(
    profile: WebProfile,
    sha256: String,
    prefix: Option<String>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    validate_sha256(&sha256).map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    if let Some(prefix) = prefix {
        validate_object_url_prefix(&prefix, &sha256)?;
    }
    let path = object_path(&profile.store, &sha256);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found(format!("object not found: {}", sha256)));
        }
        Err(error) => {
            return Err(internal_error(format!(
                "failed to open {}: {error}",
                path.display()
            )));
        }
    };
    let len = file
        .metadata()
        .await
        .map_err(|error| internal_error(format!("failed to stat {}: {error}", path.display())))?
        .len();

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map_or(ByteRange::Full, |value| parse_byte_range(value, len));
    let (status, start, end_exclusive) = match range {
        ByteRange::Full => (StatusCode::OK, 0, len),
        ByteRange::Slice { start, end } => (StatusCode::PARTIAL_CONTENT, start, end + 1),
        ByteRange::Unsatisfiable => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .map_err(|error| ApiError::internal(error.to_string()));
        }
    };
    let content_length = end_exclusive - start;
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await.map_err(|error| {
            internal_error(format!("failed to seek {}: {error}", path.display()))
        })?;
    }

    // `take` bounds the stream for range replies and pins the full reply to
    // the promised Content-Length even if the file grows mid-stream.
    let body = Body::from_stream(ReaderStream::new(file.take(content_length)));
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length)
        .header(HeaderName::from_static("x-ads-sha256"), sha256.as_str());
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{len}", end_exclusive - 1),
        );
    }
    builder
        .body(body)
        .map_err(|error| ApiError::internal(error.to_string()))
}

fn validate_object_url_prefix(prefix: &str, sha256: &str) -> std::result::Result<(), ApiError> {
    if prefix.len() != 2 || !prefix.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request(format!(
            "invalid sha256 prefix in object URL: {prefix}"
        )));
    }
    let expected = sha256
        .get(0..2)
        .ok_or_else(|| ApiError::bad_request("sha256 is too short"))?;
    if !prefix.eq_ignore_ascii_case(expected) {
        return Err(ApiError::not_found(format!(
            "object prefix {prefix} does not match sha256 {sha256}"
        )));
    }
    Ok(())
}

/// Outcome of parsing a Range header against an object of known length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ByteRange {
    Full,
    /// Inclusive byte bounds, both within the object.
    Slice {
        start: u64,
        end: u64,
    },
    Unsatisfiable,
}

/// Single-range `bytes=` parser. Multi-range and malformed headers return
/// `Full` — RFC 9110 lets a server ignore Range — while a syntactically
/// valid range that selects nothing is `Unsatisfiable` (416).
pub(crate) fn parse_byte_range(header: &str, len: u64) -> ByteRange {
    let Some(spec) = header.strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return ByteRange::Full;
    };
    match (first.is_empty(), last.is_empty()) {
        // "-n": the final n bytes.
        (true, false) => {
            let Ok(suffix) = last.parse::<u64>() else {
                return ByteRange::Full;
            };
            if suffix == 0 || len == 0 {
                return ByteRange::Unsatisfiable;
            }
            ByteRange::Slice {
                start: len.saturating_sub(suffix),
                end: len - 1,
            }
        }
        // "a-": from a to the end.
        (false, true) => {
            let Ok(start) = first.parse::<u64>() else {
                return ByteRange::Full;
            };
            if start >= len {
                return ByteRange::Unsatisfiable;
            }
            ByteRange::Slice {
                start,
                end: len - 1,
            }
        }
        // "a-b": inclusive bounds, end clamped to the object.
        (false, false) => {
            let (Ok(start), Ok(end)) = (first.parse::<u64>(), last.parse::<u64>()) else {
                return ByteRange::Full;
            };
            if start > end {
                return ByteRange::Full;
            }
            if start >= len {
                return ByteRange::Unsatisfiable;
            }
            ByteRange::Slice {
                start,
                end: end.min(len - 1),
            }
        }
        (true, true) => ByteRange::Full,
    }
}

pub(crate) async fn api_upload_object(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ObjectQuery>,
    body: Body,
) -> std::result::Result<Json<ObjectUploadResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    validate_sha256(&query.sha256).map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    let path = object_path(&profile.store, &query.sha256);
    let parent = path
        .parent()
        .ok_or_else(|| internal_error(format!("invalid object path: {}", path.display())))?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        internal_error(format!(
            "failed to create object directory {}: {error}",
            parent.display()
        ))
    })?;
    let temp_path = temp_sibling_path(&path);
    let (computed, size) =
        stream_body_to_temp(body, &temp_path, state.max_object_upload_bytes).await?;
    if computed != query.sha256 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(ApiError::bad_request(format!(
            "uploaded object hash mismatch: expected {}, computed {computed}",
            query.sha256
        )));
    }

    let lock = profile.mutation_lock.clone();
    let sha256 = query.sha256.clone();
    let temp = temp_path.clone();
    let result = run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        // Same short-circuit as the buffered handler had: a valid
        // byte-identical object already in the store is reused, not replaced.
        let reused = store.object_is_valid(&sha256, size)?;
        if reused {
            let _ = fs::remove_file(&temp);
        } else {
            store.finalize_object_temp(&temp, &sha256)?;
        }
        Ok(ObjectUploadResponse {
            sha256,
            size,
            reused,
        })
    })
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    result.map(Json)
}

/// Streams a request body to `temp_path`, hashing chunks as they arrive so
/// multi-GB uploads never sit in memory, and enforcing the configured cap
/// mid-stream. Removes the temp file itself on every error path.
pub(crate) async fn stream_body_to_temp(
    body: Body,
    temp_path: &Path,
    max_bytes: usize,
) -> std::result::Result<(String, u64), ApiError> {
    let mut file = tokio::fs::File::create(temp_path).await.map_err(|error| {
        internal_error(format!(
            "failed to create temporary object {}: {error}",
            temp_path.display()
        ))
    })?;
    let result = copy_body_hashed(&mut file, body, temp_path, max_bytes).await;
    // Deterministic close before rename/remove: Windows cannot move or
    // delete a file whose handle is still open, and `into_std` waits for
    // in-flight writes that a plain drop could leave running.
    drop(file.into_std().await);
    if result.is_err() {
        let _ = tokio::fs::remove_file(temp_path).await;
    }
    result
}

pub(crate) async fn copy_body_hashed(
    file: &mut tokio::fs::File,
    body: Body,
    temp_path: &Path,
    max_bytes: usize,
) -> std::result::Result<(String, u64), ApiError> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            ApiError::bad_request(format!("failed to read request body: {error}"))
        })?;
        size += chunk.len() as u64;
        if size > max_bytes as u64 {
            return Err(ApiError::payload_too_large(format!(
                "object upload exceeds the configured limit of {max_bytes} bytes"
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|error| {
            internal_error(format!(
                "failed to write temporary object {}: {error}",
                temp_path.display()
            ))
        })?;
    }
    file.flush().await.map_err(|error| {
        internal_error(format!(
            "failed to write temporary object {}: {error}",
            temp_path.display()
        ))
    })?;
    Ok((hex::encode(hasher.finalize()), size))
}

pub(crate) async fn api_current_status(
    State(state): State<Arc<WebState>>,
    Query(query): Query<CurrentStatusQuery>,
) -> std::result::Result<Json<Vec<CurrentStatus>>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        if let Some(category) = &query.category {
            validate_category(category)?;
        }
        if let Some(asset_code) = &query.asset_code {
            validate_asset_code(asset_code)?;
        }
        if let Some(department) = &query.department {
            validate_department(department)?;
        }
        let store = profile.store_handle.clone();
        store.current_status(
            query.category.as_deref(),
            query.asset_code.as_deref(),
            query.department.as_deref(),
        )
    })
    .await
    .map(Json)
}

pub(crate) async fn api_update_current(
    State(state): State<Arc<WebState>>,
    Json(request): Json<CurrentUpdateRequest>,
) -> std::result::Result<Json<CurrentStatus>, ApiError> {
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(request.category, request.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, request.department)?;
        if request.reset.unwrap_or(false) {
            store.reset_current_version(&department_key)
        } else {
            let version = request
                .version
                .ok_or_else(|| anyhow!("version is required unless reset=true"))?;
            store.set_current_version(&department_key, version)
        }
    })
    .await
    .map(Json)
}

pub(crate) async fn api_materialize(
    State(state): State<Arc<WebState>>,
    Json(request): Json<WorkspacePullRequest>,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    api_pull_impl(state, request).await
}

pub(crate) async fn api_pull(
    State(state): State<Arc<WebState>>,
    Json(request): Json<WorkspacePullRequest>,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    api_pull_impl(state, request).await
}

pub(crate) async fn api_restore(
    State(state): State<Arc<WebState>>,
    Json(request): Json<WorkspacePullRequest>,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    if request.version.is_none() {
        return Err(ApiError::bad_request("version is required for restore"));
    }
    api_pull_impl(state, request).await
}

pub(crate) async fn api_pull_impl(
    state: Arc<WebState>,
    request: WorkspacePullRequest,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(request.category, request.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, request.department)?;
        let selector = if request.latest.unwrap_or(false) {
            VersionSelector::Latest
        } else {
            request
                .version
                .map_or(VersionSelector::Current, VersionSelector::Version)
        };
        store.materialize(
            &profile.workspace,
            &department_key,
            selector,
            request.force.unwrap_or(false),
        )
    })
    .await
    .map(Json)
}

pub(crate) async fn api_upload_thumbnail(
    State(state): State<Arc<WebState>>,
    multipart: Multipart,
) -> std::result::Result<Json<ThumbnailRecord>, ApiError> {
    let upload = parse_thumbnail_upload(multipart, state.max_upload_bytes).await?;
    let profile = profile_for(&state, &upload.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let temp_path = unique_temp_upload_path();
        fs::write(&temp_path, &upload.bytes)
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        let result = (|| {
            let store = profile.store_handle.clone();
            let asset_key = AssetKey::new(upload.category, upload.asset_code)?;
            let department_key = DepartmentKey::new(asset_key, upload.department)?;
            store.set_thumbnail(&department_key, upload.version, &temp_path)
        })();
        let _ = fs::remove_file(&temp_path);
        result
    })
    .await
    .map(Json)
}

pub(crate) async fn api_import_thumbnail(
    State(state): State<Arc<WebState>>,
    Json(request): Json<ThumbnailImportRequest>,
) -> std::result::Result<Json<ThumbnailRecord>, ApiError> {
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        store.import_thumbnail_info(&request.thumbnail)?;
        Ok(request.thumbnail)
    })
    .await
    .map(Json)
}

pub(crate) async fn api_thumbnail_url(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ThumbnailUrlQuery>,
) -> std::result::Result<Json<String>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(query.category, query.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, query.department)?;
        let selector = if query.latest.unwrap_or(false) {
            VersionSelector::Latest
        } else {
            query
                .version
                .map_or(VersionSelector::Current, VersionSelector::Version)
        };
        store.thumbnail_url(&department_key, selector, query.remote_base_url.as_deref())
    })
    .await
    .map(Json)
}

pub(crate) async fn api_resolve(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ResolveQuery>,
    headers: HeaderMap,
) -> std::result::Result<Json<ResolveOutcome>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    let profile_count = state.profiles.len();
    run_store_read(move || {
        let mode = parse_resolve_mode(query.mode.as_deref().unwrap_or("auto"))?;
        let asset_path = AssetPath::parse(&query.asset_path)?;
        let store = profile.store_handle.clone();
        let mut remote_base_url = query.remote_base_url.clone();
        if remote_base_url.is_none()
            && mode != ResolveMode::Local
            && store.remote_base_url()?.is_none()
        {
            remote_base_url = Some(served_object_base_url(
                &headers,
                &profile.name,
                profile_count,
            )?);
        }
        store.resolve_asset_path(
            &profile.workspace,
            &asset_path,
            mode,
            remote_base_url.as_deref(),
        )
    })
    .await
    .map(Json)
}

pub(crate) async fn api_wips(
    State(state): State<Arc<WebState>>,
    Query(query): Query<WipsQuery>,
) -> std::result::Result<Json<WipsResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(query.category, query.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, query.department)?;
        let wips = store.list_wips(&department_key)?;
        Ok(WipsResponse { wips })
    })
    .await
    .map(Json)
}

/// Registers a WIP micro-version against a served store — the remote
/// counterpart of `ads wip add`, whose exclusive RocksDB open cannot coexist
/// with `ads serve`. Objects must already be in the store (uploaded via
/// PUT /api/object, same as push); only metadata is written here.
pub(crate) async fn api_wip(
    State(state): State<Arc<WebState>>,
    payload: std::result::Result<Json<WipImportRequest>, JsonRejection>,
) -> std::result::Result<Json<WipOutcome>, ApiError> {
    let Json(request) = payload.map_err(json_rejection_error)?;
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(request.category, request.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, request.department)?;
        let mut missing = Vec::new();
        for entry in &request.manifest.entries {
            validate_sha256(&entry.sha256)?;
            validate_manifest_relative_path(&entry.relative_path)?;
            if !store.object_is_valid(&entry.sha256, entry.size)? {
                missing.push(entry.sha256.as_str());
            }
        }
        if !missing.is_empty() {
            const LISTED: usize = 10;
            let suffix = if missing.len() > LISTED {
                format!(" (and {} more)", missing.len() - LISTED)
            } else {
                String::new()
            };
            bail!(
                "objects missing or invalid for wip: {}{suffix}",
                missing[..missing.len().min(LISTED)].join(", ")
            );
        }
        let source_path = request.source_path.unwrap_or_else(|| {
            format!(
                "{}/{}/{}",
                department_key.asset_key.category,
                department_key.asset_key.asset_code,
                department_key.department
            )
        });
        store.add_wip_from_manifest(&department_key, &request.manifest, source_path)
    })
    .await
    .map(Json)
}

pub(crate) async fn api_promote(
    State(state): State<Arc<WebState>>,
    Json(request): Json<PromoteRequest>,
) -> std::result::Result<Json<PromoteResponse>, ApiError> {
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        let asset_key = AssetKey::new(request.category, request.asset_code)?;
        let department_key = DepartmentKey::new(asset_key, request.department)?;
        let wip = match request.wip_seq {
            Some(seq) => store.get_wip(&department_key, seq)?,
            None => store.wip_head(&department_key)?.ok_or_else(|| {
                anyhow!(
                    "no wip versions to promote for {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                )
            })?,
        };
        // Same gate as `ads publish promote`: validate by default, reject on
        // errors, and pass warnings through so the caller can surface them.
        let validation = if request.no_validate.unwrap_or(false) {
            None
        } else {
            let manifest = store.get_manifest(&wip.manifest_hash)?;
            let report = validate_manifest_references(
                &store,
                format!(
                    "wip seq {} of {}/{}/{}",
                    wip.seq,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                ),
                &manifest,
            )?;
            if !report.errors.is_empty() {
                bail!(
                    "promote validation gate failed for {}: {}",
                    report.target,
                    report.errors.join("; ")
                );
            }
            Some(report)
        };
        let outcome = store.promote_wip(&department_key, Some(wip.seq))?;
        Ok(PromoteResponse {
            outcome,
            validation,
        })
    })
    .await
    .map(Json)
}

pub(crate) async fn api_gc(
    State(state): State<Arc<WebState>>,
    Json(request): Json<GcRequest>,
) -> std::result::Result<Json<GcOutcome>, ApiError> {
    let profile = profile_for(&state, &request.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        let store = profile.store_handle.clone();
        store.gc(
            request.retention.unwrap_or(DEFAULT_WIP_RETENTION),
            std::time::Duration::from_secs(
                request.grace_hours.unwrap_or(DEFAULT_GC_GRACE_HOURS) * 3600,
            ),
            request.dry_run.unwrap_or(false),
        )
    })
    .await
    .map(Json)
}

#[derive(Debug)]
pub(crate) struct ThumbnailUpload {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    version: VersionId,
    bytes: Vec<u8>,
}

pub(crate) async fn parse_thumbnail_upload(
    mut multipart: Multipart,
    max_upload_bytes: usize,
) -> std::result::Result<ThumbnailUpload, ApiError> {
    let mut profile = None;
    let mut category = None;
    let mut asset_code = None;
    let mut department = None;
    let mut version = None;
    let mut bytes = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "profile" => profile = Some(read_multipart_text(field).await?),
            "category" => category = Some(read_multipart_text(field).await?),
            "asset_code" => asset_code = Some(read_multipart_text(field).await?),
            "department" => department = Some(read_multipart_text(field).await?),
            "version" => {
                let value = read_multipart_text(field).await?;
                version = Some(VersionId::from_str(&value).map_err(|error| {
                    ApiError::bad_request(format!("invalid version `{value}`: {error}"))
                })?);
            }
            "file" | "image" => {
                let data = field
                    .bytes()
                    .await
                    .map_err(|error| ApiError::bad_request(error.to_string()))?;
                if data.len() > max_upload_bytes {
                    return Err(ApiError::bad_request("thumbnail upload exceeds size limit"));
                }
                bytes = Some(data.to_vec());
            }
            _ => {}
        }
    }

    Ok(ThumbnailUpload {
        profile: required_upload_field(profile, "profile")?,
        category: required_upload_field(category, "category")?,
        asset_code: required_upload_field(asset_code, "asset_code")?,
        department: required_upload_field(department, "department")?,
        version: version.ok_or_else(|| ApiError::bad_request("missing multipart field version"))?,
        bytes: bytes.ok_or_else(|| ApiError::bad_request("missing multipart file field"))?,
    })
}

pub(crate) async fn read_multipart_text(
    field: axum::extract::multipart::Field<'_>,
) -> std::result::Result<String, ApiError> {
    field
        .text()
        .await
        .map(|value| value.trim().to_string())
        .map_err(|error| ApiError::bad_request(error.to_string()))
}

pub(crate) fn required_upload_field(
    value: Option<String>,
    field: &str,
) -> std::result::Result<String, ApiError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request(format!("missing multipart field {field}")))
}

pub(crate) async fn run_store_read<T, F>(f: F) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(map_domain_error)
}

pub(crate) async fn run_store_write<T, F>(
    lock: Arc<Mutex<()>>,
    f: F,
) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _guard = lock
            .lock()
            .map_err(|_| anyhow!("profile mutation lock poisoned"))?;
        f()
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(map_domain_error)
}

/// Maps a domain error chain onto the HTTP taxonomy. A `NotFound` marker
/// anywhere in the chain is a 404. An `io::Error` or `rocksdb::Error` in the
/// chain means an unexpected infrastructure failure (rocksdb::Error is a
/// plain string wrapper with no io::Error source, so ENOSPC and corruption
/// surface only as it): 500 with a generic body, because those messages
/// carry server filesystem paths that must not reach clients; the full chain
/// goes to the server log instead. Everything else is a user-actionable
/// input error and keeps its message as a 400 (the troubleshooting docs
/// quote some of these verbatim).
pub(crate) fn map_domain_error(error: anyhow::Error) -> ApiError {
    if error.chain().any(|cause| cause.is::<NotFound>()) {
        return ApiError::not_found(format!("{error:#}"));
    }
    if error
        .chain()
        .any(|cause| cause.is::<std::io::Error>() || cause.is::<rocksdb::Error>())
    {
        eprintln!("internal error: {error:#}");
        return ApiError::internal("internal server error");
    }
    ApiError::bad_request(format!("{error:#}"))
}

/// Category filters select whole path segments: `prop` matches `prop` and
/// `prop/vehicle` but not `propx`.
fn category_filter_matches(category: &str, filter: &str) -> bool {
    category
        .strip_prefix(filter)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

pub(crate) fn build_asset_cards(store: &Store, query: &AssetsQuery) -> Result<AssetsResponse> {
    if let Some(category) = &query.category {
        validate_category(category)?;
    }
    if let Some(asset_code) = &query.asset_code {
        validate_asset_code(asset_code)?;
    }
    if let Some(department) = &query.department {
        validate_department(department)?;
    }

    let versions = store.list_versions(
        None,
        query.asset_code.as_deref(),
        query.department.as_deref(),
    )?;
    let statuses = store.current_status(
        None,
        query.asset_code.as_deref(),
        query.department.as_deref(),
    )?;
    let status_by_department = statuses
        .into_iter()
        .map(|status| (status.department_key.clone(), status))
        .collect::<BTreeMap<_, _>>();
    let mut by_department: BTreeMap<DepartmentKey, Vec<VersionRecord>> = BTreeMap::new();
    for version in versions {
        by_department
            .entry(version.department_key.clone())
            .or_default()
            .push(version);
    }

    let q = query.q.as_ref().map(|value| value.to_ascii_lowercase());
    let mut assets = Vec::new();
    for (department_key, versions) in by_department {
        if query.category.as_ref().is_some_and(|category| {
            !category_filter_matches(&department_key.asset_key.category, category)
        }) {
            continue;
        }

        if let Some(q) = &q {
            let haystack = format!(
                "{}/{}/{}",
                department_key.asset_key.category,
                department_key.asset_key.asset_code,
                department_key.department
            )
            .to_ascii_lowercase();
            if !haystack.contains(q) {
                continue;
            }
        }

        let latest_record = versions.iter().max_by_key(|record| record.version);
        let status = status_by_department
            .get(&department_key)
            .cloned()
            .unwrap_or(CurrentStatus {
                department_key: department_key.clone(),
                current: None,
                latest: latest_record.map(|record| record.version),
                explicit: false,
            });
        let thumbnail = status.current.and_then(|version| {
            store
                .thumbnail_info(&department_key, VersionSelector::Version(version))
                .ok()
        });
        let thumbnail_url = thumbnail.as_ref().and_then(|record| {
            store
                .thumbnail_url(
                    &department_key,
                    VersionSelector::Version(record.version),
                    None,
                )
                .ok()
        });
        let thumbnail_sha256 = thumbnail.as_ref().map(|record| record.sha256.clone());
        let thumbnail_mime_type = thumbnail.as_ref().map(|record| record.mime_type.clone());

        assets.push(AssetCardDto {
            category: department_key.asset_key.category,
            asset_code: department_key.asset_key.asset_code,
            department: department_key.department,
            current: status.current,
            latest: status.latest,
            explicit_current: status.explicit,
            version_count: versions.len(),
            latest_created_at: latest_record.map(|record| record.created_at.clone()),
            latest_file_count: latest_record.map(|record| record.file_count),
            latest_total_bytes: latest_record.map(|record| record.total_bytes),
            thumbnail_url,
            thumbnail_sha256,
            thumbnail_mime_type,
        });
    }
    assets.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then(left.asset_code.cmp(&right.asset_code))
            .then(left.department.cmp(&right.department))
    });
    Ok(AssetsResponse { assets })
}

pub(crate) fn profile_for(
    state: &WebState,
    name: &str,
) -> std::result::Result<WebProfile, ApiError> {
    state
        .profiles
        .get(name)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("profile not found: {name}")))
}

fn profile_for_object_path(
    state: &WebState,
    name: Option<&str>,
) -> std::result::Result<WebProfile, ApiError> {
    if let Some(name) = name {
        return profile_for(state, name);
    }
    let mut profiles = state.profiles.values();
    let Some(profile) = profiles.next().cloned() else {
        return Err(ApiError::not_found("profile not found"));
    };
    if profiles.next().is_some() {
        return Err(ApiError::bad_request(
            "object URL is ambiguous for multiple profiles; use /objects/<profile>/sha256/<prefix>/<sha256>",
        ));
    }
    Ok(profile)
}

fn served_object_base_url(
    headers: &HeaderMap,
    profile_name: &str,
    profile_count: usize,
) -> Result<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("Host header is required to infer remote object base URL"))?;
    let proto = headers
        .get(HeaderName::from_static("x-forwarded-proto"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http");
    if proto != "http" && proto != "https" {
        bail!("invalid X-Forwarded-Proto header: {proto}");
    }
    let origin = format!("{proto}://{host}");
    if profile_count == 1 {
        Ok(format!("{origin}/objects/sha256"))
    } else {
        Ok(format!("{origin}/objects/{profile_name}/sha256"))
    }
}

pub(crate) fn parse_resolve_mode(value: &str) -> Result<ResolveMode> {
    match value {
        "auto" => Ok(ResolveMode::Auto),
        "local" => Ok(ResolveMode::Local),
        "remote" => Ok(ResolveMode::Remote),
        _ => bail!("resolve mode must be auto, local, or remote: {value}"),
    }
}

pub(crate) fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(crate) fn validate_profile_name(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("profile name must not be empty");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("profile name may only contain ASCII letters, digits, '-', '_', and '.'");
    }
    Ok(())
}

pub(crate) fn unique_temp_upload_path() -> PathBuf {
    let nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp_micros());
    std::env::temp_dir().join(format!(
        "ads-thumbnail-{}-{nanos}.upload",
        std::process::id()
    ))
}
