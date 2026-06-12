//! HTTP API and WebApp server: axum routes, handlers, and serve state.

use std::collections::BTreeMap;
use std::fs::{self};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

use crate::*;

#[derive(Clone)]
pub(crate) struct WebState {
    auth_token: String,
    profiles: Arc<BTreeMap<String, WebProfile>>,
    max_upload_bytes: usize,
    max_object_upload_bytes: usize,
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
    let max_object_upload_bytes = state.max_object_upload_bytes;
    let api = Router::new()
        .route("/profiles", get(api_profiles))
        .route("/assets", get(api_assets))
        .route("/asset", get(api_asset))
        .route("/versions", get(api_versions))
        .route("/version", get(api_version_info).put(api_import_version))
        .route("/object/status", get(api_object_status))
        .route(
            "/object",
            get(api_object)
                .put(api_upload_object)
                .layer(DefaultBodyLimit::max(max_object_upload_bytes)),
        )
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
        .route("/promote", post(api_promote))
        .route("/gc", post(api_gc))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api_auth_middleware,
        ));

    Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .nest("/api", api)
        .with_state(state)
        .layer(CorsLayer::permissive())
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
    Json(request): Json<VersionImportRequest>,
) -> std::result::Result<Json<VersionRecord>, ApiError> {
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
) -> std::result::Result<Response, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        validate_sha256(&query.sha256)?;
        let path = object_path(&profile.store, &query.sha256);
        if !path.exists() {
            bail!("object not found: {}", query.sha256);
        }
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        Ok((query.sha256, bytes))
    })
    .await
    .map(|(sha256, bytes)| {
        let mut response = bytes.into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        if let Ok(value) = HeaderValue::from_str(&sha256) {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-ads-sha256"), value);
        }
        response
    })
}

pub(crate) async fn api_upload_object(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ObjectQuery>,
    body: Bytes,
) -> std::result::Result<Json<ObjectUploadResponse>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    let lock = profile.mutation_lock.clone();
    run_store_write(lock, move || {
        validate_sha256(&query.sha256)?;
        let store = profile.store_handle.clone();
        let size = body.len() as u64;
        let reused = store.object_is_valid(&query.sha256, size)?;
        if !reused {
            store.write_object_bytes(&query.sha256, &body)?;
        }
        Ok(ObjectUploadResponse {
            sha256: query.sha256,
            size,
            reused,
        })
    })
    .await
    .map(Json)
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
) -> std::result::Result<Json<ResolveOutcome>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let mode = parse_resolve_mode(query.mode.as_deref().unwrap_or("auto"))?;
        let asset_path = AssetPath::parse(&query.asset_path)?;
        let store = profile.store_handle.clone();
        store.resolve_asset_path(&profile.workspace, &asset_path, mode, None)
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
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))
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
    .map_err(|error| ApiError::bad_request(format!("{error:#}")))
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
            !department_key
                .asset_key
                .category
                .starts_with(category.as_str())
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
