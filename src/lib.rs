use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use axum::http::{HeaderValue, StatusCode};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobMatcher};
use rocksdb::{DB, Direction, IteratorMode, Options, WriteBatch};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

mod remote;
mod web;
mod webapp;

pub(crate) use remote::*;
pub(crate) use web::*;
pub(crate) use webapp::*;

const SCHEMA_VERSION: &str = "8";
const DB_DIR: &str = "db";
const OBJECTS_DIR: &str = "objects";
const SHA256_DIR: &str = "sha256";
const CACHE_DIR: &str = ".ads-cache";
const MANIFESTS_DIR: &str = "manifests";
/// Completeness marker written inside a manifest view before it is renamed
/// into place. Views from before atomic materialization used a sibling
/// `<hash>.complete` file instead; both are still honored on read.
const VIEW_COMPLETE_MARKER: &str = ".ads-complete";
const STAGING_DIR: &str = ".ads-staging";
const USD_EXTENSIONS: &[&str] = &["usd", "usda", "usdc", "usdz"];
/// Formats that can carry relative references to sibling files and therefore
/// resolve through the eagerly materialized manifest view. Everything else is
/// a leaf (textures, volumes, caches, ...) and resolves lazily to its flat
/// blob cache path — one file copied per request instead of the whole
/// version.
const VIEW_EXTENSIONS: &[&str] = &["usd", "usda", "usdc", "usdz", "mtlx"];
/// Defaults shared by `ads gc` and the `/api/gc` endpoint.
const DEFAULT_WIP_RETENTION: usize = 20;
const DEFAULT_GC_GRACE_HOURS: u64 = 24;

#[derive(Parser, Debug)]
#[command(
    name = "ads",
    version,
    about = "Folder-based asset versioning with content-addressed storage"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a new asset store.
    Init {
        /// Store root path.
        store: PathBuf,
        /// Remote object base URL, for example https://assets.example.com/objects/sha256.
        #[arg(long = "remote-base-url")]
        remote_base_url: Option<String>,
    },
    /// Register a version from a source folder.
    Add {
        /// Store root path. Required unless --server is used.
        #[arg(long)]
        store: Option<PathBuf>,
        /// ADS Web/API server base URL. When set, registration writes through
        /// the server instead of opening the local store.
        #[arg(long)]
        server: Option<String>,
        /// Bearer token for the ADS Web/API server. Can also be ADS_WEB_TOKEN.
        #[arg(long = "auth-token")]
        auth_token: Option<String>,
        /// Remote profile name for --server. Defaults to "default".
        #[arg(long)]
        profile: Option<String>,
        /// Workspace root, used together with --version to locate the
        /// conventional <category>/<asset-code>/<department>/v### folder.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version number, for example 3 or v003. Defaults to the next
        /// version when --source is used.
        #[arg(long)]
        version: Option<VersionId>,
        /// Arbitrary source folder to register (schema v8: no standard
        /// workspace layout is required).
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Asset-level operations.
    Asset {
        #[command(subcommand)]
        command: AssetCommands,
    },
    /// Current-version pointer operations.
    Current {
        #[command(subcommand)]
        command: CurrentCommands,
    },
    /// Thumbnail operations.
    Thumbnail {
        #[command(subcommand)]
        command: ThumbnailCommands,
    },
    /// List versions.
    List {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Optional category filter.
        #[arg(long)]
        category: Option<String>,
        /// Optional asset-code filter.
        #[arg(long = "asset-code")]
        asset_code: Option<String>,
        /// Optional department filter.
        #[arg(long)]
        department: Option<String>,
    },
    /// Show asset or version details.
    Info {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: Option<String>,
        /// Optional version, for example v001.
        #[arg(long)]
        version: Option<VersionId>,
    },
    /// Restore a version to a destination folder.
    Checkout {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to restore. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Restore the latest version instead of the current version.
        #[arg(long)]
        latest: bool,
        /// Replace an existing non-empty destination.
        #[arg(long)]
        force: bool,
        /// Destination folder.
        dest: PathBuf,
    },
    /// WIP micro-version stream operations (schema v8).
    Wip {
        #[command(subcommand)]
        command: WipCommands,
    },
    /// Garbage-collect unreferenced objects and expired WIP versions.
    Gc {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Newest WIP micro-versions to keep per department.
        #[arg(long, default_value_t = DEFAULT_WIP_RETENTION)]
        retention: usize,
        /// Grace period in hours: unreferenced objects newer than this are kept.
        #[arg(long = "grace-hours", default_value_t = DEFAULT_GC_GRACE_HOURS)]
        grace_hours: u64,
        /// Report what would be deleted without deleting anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Workspace cache maintenance.
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
    /// Fetch a version and missing objects from a remote ADS server into a local store.
    Fetch(FetchArgs),
    /// Sync remote assets and missing objects into a local store.
    Sync(SyncArgs),
    /// Push a local version and missing objects to a remote ADS server.
    Push(PushArgs),
    /// Seed the department work folder with a version's content.
    Materialize {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to materialize. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Materialize the latest version instead of the current version.
        #[arg(long)]
        latest: bool,
        /// Replace a work folder whose content differs.
        #[arg(long)]
        force: bool,
    },
    /// Resolve an ads:// asset path to a local path or remote object URL.
    Resolve {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Resolution mode.
        #[arg(long, value_enum, default_value_t = ResolveMode::Auto)]
        mode: ResolveMode,
        /// Override the store remote object base URL.
        #[arg(long = "remote-base-url")]
        remote_base_url: Option<String>,
        /// Asset path such as ads://hero/model/hero.usd or ads://char/hero/model/hero.usd?v=2 (v002 form also accepted).
        asset_path: String,
    },
    /// Set the store remote object base URL used by resolve --mode remote/auto.
    SetRemote {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Remote object base URL, for example https://assets.example.com/objects/sha256.
        #[arg(long = "remote-base-url")]
        remote_base_url: String,
    },
    /// Serve the asset browser WebApp and JSON API.
    Serve {
        /// Bind address for the HTTP server.
        #[arg(long, default_value = "0.0.0.0:8787")]
        bind: SocketAddr,
        /// Bearer token required for /api/* requests. Can also be ADS_WEB_TOKEN.
        #[arg(long = "auth-token", env = "ADS_WEB_TOKEN")]
        auth_token: Option<String>,
        /// Allowed profile in name=store::workspace form. Repeatable.
        #[arg(long = "profile")]
        profiles: Vec<String>,
        /// Store root path for a single default profile.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Workspace root for a single default profile.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Maximum thumbnail upload size in MiB.
        #[arg(long = "max-upload-mb", default_value_t = 10)]
        max_upload_mb: u64,
        /// Maximum remote object upload size in MiB.
        #[arg(long = "max-object-upload-mb", default_value_t = 1024)]
        max_object_upload_mb: u64,
        /// Origin allowed to call the API from a browser, for example
        /// https://tools.example.com. Repeatable. Without this flag no CORS
        /// headers are sent (the bundled WebApp is served same-origin).
        #[arg(long = "cors-allow-origin")]
        cors_allow_origins: Vec<String>,
    },
    /// Public publish folder operations.
    Publish {
        #[command(subcommand)]
        command: PublishCommands,
    },
    /// Verify metadata and object content.
    Verify {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
    },
}

#[derive(Args, Debug)]
struct FetchArgs {
    /// Remote ADS server base URL, for example http://ads-server:8787.
    #[arg(long)]
    server: String,
    /// Bearer token for the remote ADS server. Can also be ADS_WEB_TOKEN.
    #[arg(long = "auth-token", env = "ADS_WEB_TOKEN")]
    auth_token: String,
    /// Remote profile name.
    #[arg(long, default_value = "main")]
    profile: String,
    /// Local store root. It is initialized if missing.
    #[arg(long)]
    store: PathBuf,
    /// Workspace root for optional materialization.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Asset category.
    #[arg(long)]
    category: String,
    /// Asset code.
    #[arg(long = "asset-code")]
    asset_code: String,
    /// Work department such as model, rig, anim, fx, or lookdev.
    #[arg(long)]
    department: String,
    /// Version to fetch. Defaults to the remote current version.
    #[arg(long)]
    version: Option<VersionId>,
    /// Fetch the remote latest version instead of current.
    #[arg(long)]
    latest: bool,
    /// Restore the fetched version into the local workspace.
    #[arg(long)]
    materialize: bool,
    /// Replace a different existing workspace version folder when materializing.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct SyncArgs {
    /// Remote ADS server base URL, for example http://ads-server:8787.
    #[arg(long)]
    server: String,
    /// Bearer token for the remote ADS server. Can also be ADS_WEB_TOKEN.
    #[arg(long = "auth-token", env = "ADS_WEB_TOKEN")]
    auth_token: String,
    /// Remote profile name.
    #[arg(long, default_value = "main")]
    profile: String,
    /// Local store root. It is initialized if missing.
    #[arg(long)]
    store: PathBuf,
    /// Workspace root for optional materialization.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Optional category filter.
    #[arg(long)]
    category: Option<String>,
    /// Optional asset-code filter.
    #[arg(long = "asset-code")]
    asset_code: Option<String>,
    /// Optional department filter.
    #[arg(long)]
    department: Option<String>,
    /// Sync remote latest versions instead of current versions.
    #[arg(long)]
    latest: bool,
    /// Sync every remote version matching the filters.
    #[arg(long = "all-versions")]
    all_versions: bool,
    /// Restore synced current/latest versions into the local workspace.
    #[arg(long)]
    materialize: bool,
    /// Replace work folders whose content differs when materializing.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct PushArgs {
    /// Remote ADS server base URL, for example http://ads-server:8787.
    #[arg(long)]
    server: String,
    /// Bearer token for the remote ADS server. Can also be ADS_WEB_TOKEN.
    #[arg(long = "auth-token", env = "ADS_WEB_TOKEN")]
    auth_token: String,
    /// Remote profile name.
    #[arg(long, default_value = "main")]
    profile: String,
    /// Local store root.
    #[arg(long)]
    store: PathBuf,
    /// Asset category.
    #[arg(long)]
    category: String,
    /// Asset code.
    #[arg(long = "asset-code")]
    asset_code: String,
    /// Work department such as model, rig, anim, fx, or lookdev.
    #[arg(long)]
    department: String,
    /// Version to push. Defaults to the local current version.
    #[arg(long)]
    version: Option<VersionId>,
    /// Push the local latest version instead of current.
    #[arg(long)]
    latest: bool,
    /// Set the remote current pointer to the pushed version.
    #[arg(long = "set-current")]
    set_current: bool,
}

#[derive(Subcommand, Debug)]
enum PublishCommands {
    /// Validate the publish reference policy for a version, the WIP head, or
    /// a source folder.
    Validate {
        /// Store root path (required unless --source is used).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Asset category.
        #[arg(long)]
        category: Option<String>,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: Option<String>,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: Option<String>,
        /// Publish version to validate. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Validate the latest version instead of the current version.
        #[arg(long)]
        latest: bool,
        /// Validate the WIP head instead of a publish version.
        #[arg(long)]
        wip: bool,
        /// Validate a specific WIP sequence.
        #[arg(long = "wip-seq")]
        wip_seq: Option<u64>,
        /// Validate an arbitrary source folder before registration.
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Promote a WIP micro-version to the next publish version (metadata only).
    Promote {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// WIP sequence to promote. Defaults to the WIP head.
        #[arg(long = "wip-seq")]
        wip_seq: Option<u64>,
        /// Skip the publish reference validation gate.
        #[arg(long = "no-validate")]
        no_validate: bool,
    },
}

#[derive(Subcommand, Debug)]
enum CacheCommands {
    /// Delete manifest views, blobs, and stale staging runs that latest,
    /// current, and WIP heads no longer reference. The cache rebuilds on
    /// demand, so this is safe at any time and works while `ads serve` runs.
    Gc {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Staging runs older than this many hours are removed.
        #[arg(long = "staging-hours", default_value_t = 24)]
        staging_hours: u64,
        /// Report what would be deleted without deleting anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum WipCommands {
    /// Register a WIP micro-version from a source folder.
    Add {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Source folder whose contents become the new WIP head.
        #[arg(long)]
        source: PathBuf,
    },
    /// List WIP micro-versions for a department.
    List {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
    },
}

#[derive(Subcommand, Debug)]
enum AssetCommands {
    /// Create an asset record and its workspace asset folder.
    Create {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Workspace root. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
    },
    /// Show asset metadata and registered versions.
    Log {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
    },
}

#[derive(Subcommand, Debug)]
enum CurrentCommands {
    /// Print the current version for a department.
    Get {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
    },
    /// Pin the current version for a department.
    Set {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to make current.
        #[arg(long)]
        version: VersionId,
    },
    /// Clear an explicit current pin so current follows latest.
    Reset {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
    },
    /// List current/latest pointers.
    Status {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Optional category filter.
        #[arg(long)]
        category: Option<String>,
        /// Optional asset-code filter.
        #[arg(long = "asset-code")]
        asset_code: Option<String>,
        /// Optional department filter.
        #[arg(long)]
        department: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ThumbnailCommands {
    /// Attach or replace a thumbnail for a registered version.
    Set {
        /// Store root path. Required unless --server is used.
        #[arg(long)]
        store: Option<PathBuf>,
        /// ADS Web/API server base URL. When set, registration writes through
        /// the server instead of opening the local store.
        #[arg(long)]
        server: Option<String>,
        /// Bearer token for the ADS Web/API server. Can also be ADS_WEB_TOKEN.
        #[arg(long = "auth-token")]
        auth_token: Option<String>,
        /// Remote profile name for --server. Defaults to "default".
        #[arg(long)]
        profile: Option<String>,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to attach the thumbnail to.
        #[arg(long)]
        version: VersionId,
        /// Source thumbnail image. PNG, JPEG, and WebP are supported.
        image: PathBuf,
    },
    /// Copy a thumbnail image to a destination path.
    Get {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to fetch. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Fetch the latest version thumbnail instead of the current version.
        #[arg(long)]
        latest: bool,
        /// Replace an existing destination file.
        #[arg(long)]
        force: bool,
        /// Destination image path.
        dest: PathBuf,
    },
    /// Show thumbnail metadata.
    Info {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to inspect. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Inspect the latest version thumbnail instead of the current version.
        #[arg(long)]
        latest: bool,
    },
    /// List thumbnail metadata.
    List {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Optional category filter.
        #[arg(long)]
        category: Option<String>,
        /// Optional asset-code filter.
        #[arg(long = "asset-code")]
        asset_code: Option<String>,
        /// Optional department filter.
        #[arg(long)]
        department: Option<String>,
    },
    /// Print the remote object URL for a thumbnail.
    Url {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to resolve. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Resolve the latest version thumbnail instead of the current version.
        #[arg(long)]
        latest: bool,
        /// Override the store remote object base URL.
        #[arg(long = "remote-base-url")]
        remote_base_url: Option<String>,
    },
    /// Remove thumbnail metadata for a version.
    Remove {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
        /// Asset category. Supports nested paths such as aaa/bbb/ccc.
        #[arg(long)]
        category: String,
        /// Asset code.
        #[arg(long = "asset-code")]
        asset_code: String,
        /// Work department such as model, rig, anim, fx, or lookdev.
        #[arg(long)]
        department: String,
        /// Version to remove the thumbnail from. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Remove the latest version thumbnail instead of the current version.
        #[arg(long)]
        latest: bool,
    },
}

pub fn run() -> Result<()> {
    run_with_args(std::env::args_os())
}

pub fn run_with_args<I, T>(args: I) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::parse_from(args);
    match cli.command {
        Commands::Init {
            store,
            remote_base_url,
        } => {
            let store_handle = Store::init(&store)?;
            if let Some(remote_base_url) = remote_base_url {
                store_handle.set_remote_base_url(&remote_base_url)?;
            }
            println!("initialized store at {}", store.display());
        }
        Commands::Add {
            store,
            server,
            auth_token,
            profile,
            workspace,
            category,
            asset_code,
            department,
            version,
            source,
        } => {
            let asset_key = AssetKey::new(category, asset_code)?;
            let department_key = DepartmentKey::new(asset_key, department)?;
            let outcome = if let Some(server) = server {
                let auth_token = resolve_remote_auth_token(auth_token.as_deref())?;
                let remote = RemoteClient::new(&server, &auth_token)?;
                let profile = profile.as_deref().unwrap_or("default");
                add_remote_source(
                    &remote,
                    profile,
                    source.as_deref(),
                    &department_key,
                    version,
                )?
            } else {
                let store = store
                    .as_deref()
                    .ok_or_else(|| anyhow!("--store is required unless --server is used"))?;
                let store = Store::open(store)?;
                match source {
                    Some(source) => {
                        let version = match version {
                            Some(version) => version,
                            None => store.next_version(&department_key)?,
                        };
                        store.add_version_source(&source, &department_key, version)?
                    }
                    None => {
                        let version = version.ok_or_else(|| {
                            anyhow!(
                                "either --source or --version (with a workspace version folder) is required"
                            )
                        })?;
                        let workspace = workspace_root(workspace)?;
                        store.add_version_folder(&workspace, &department_key, version)?
                    }
                }
            };
            if outcome.created {
                println!(
                    "created {} {}/{}/{} files={} bytes={} manifest={}",
                    outcome.version,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    outcome.file_count,
                    outcome.total_bytes,
                    outcome.manifest_hash
                );
            } else {
                println!(
                    "reused {} {}/{}/{} files={} bytes={} manifest={}",
                    outcome.version,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    outcome.file_count,
                    outcome.total_bytes,
                    outcome.manifest_hash
                );
            }
        }
        Commands::Wip { command } => match command {
            WipCommands::Add {
                store,
                category,
                asset_code,
                department,
                source,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open(&store)?;
                let outcome = store.add_wip_from_source(&source, &department_key)?;
                println!(
                    "{} wip seq={} {}/{}/{} files={} bytes={} manifest={}",
                    if outcome.created {
                        "registered"
                    } else {
                        "unchanged"
                    },
                    outcome.seq,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    outcome.file_count,
                    outcome.total_bytes,
                    outcome.manifest_hash
                );
            }
            WipCommands::List {
                store,
                category,
                asset_code,
                department,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open_read_only(&store)?;
                let records = store.list_wips(&department_key)?;
                println!("{}", serde_json::to_string_pretty(&records)?);
            }
        },
        Commands::Gc {
            store,
            retention,
            grace_hours,
            dry_run,
        } => {
            let store = Store::open(&store)?;
            let outcome = store.gc(
                retention,
                std::time::Duration::from_secs(grace_hours * 3600),
                dry_run,
            )?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        Commands::Cache { command } => match command {
            CacheCommands::Gc {
                store,
                workspace,
                staging_hours,
                dry_run,
            } => {
                let workspace = workspace_root(workspace)?;
                let store = Store::open_read_only(&store)?;
                let outcome = store.cache_gc(
                    &workspace,
                    std::time::Duration::from_secs(staging_hours * 3600),
                    dry_run,
                )?;
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            }
        },
        Commands::Asset { command } => match command {
            AssetCommands::Create {
                store,
                workspace,
                category,
                asset_code,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let workspace = workspace_root(workspace)?;
                let store = Store::open(&store)?;
                let outcome = store.create_asset(&workspace, &asset_key)?;
                println!(
                    "created asset {}/{} at {}",
                    outcome.asset.asset_key.category,
                    outcome.asset.asset_key.asset_code,
                    outcome.path.display()
                );
            }
            AssetCommands::Log {
                store,
                category,
                asset_code,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let store = Store::open_read_only(&store)?;
                let info = store.asset_info(&asset_key)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
        },
        Commands::Current { command } => match command {
            CurrentCommands::Get {
                store,
                category,
                asset_code,
                department,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open_read_only(&store)?;
                let status = store.current_status_for_department(&department_key)?;
                let current = status.current.ok_or_else(|| {
                    anyhow!(
                        "department has no registered versions: {}/{}/{}",
                        department_key.asset_key.category,
                        department_key.asset_key.asset_code,
                        department_key.department
                    )
                })?;
                println!("{current}");
            }
            CurrentCommands::Set {
                store,
                category,
                asset_code,
                department,
                version,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open(&store)?;
                let status = store.set_current_version(&department_key, version)?;
                println!(
                    "current set to {} {}/{}/{} latest={}",
                    version,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    format_optional_version(status.latest)
                );
            }
            CurrentCommands::Reset {
                store,
                category,
                asset_code,
                department,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open(&store)?;
                let status = store.reset_current_version(&department_key)?;
                println!(
                    "current reset {}/{}/{} current={} latest={}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    format_optional_version(status.current),
                    format_optional_version(status.latest)
                );
            }
            CurrentCommands::Status {
                store,
                category,
                asset_code,
                department,
            } => {
                if let Some(category) = &category {
                    validate_category(category)?;
                }
                if let Some(asset_code) = &asset_code {
                    validate_asset_code(asset_code)?;
                }
                if let Some(department) = &department {
                    validate_department(department)?;
                }
                let store = Store::open_read_only(&store)?;
                let statuses = store.current_status(
                    category.as_deref(),
                    asset_code.as_deref(),
                    department.as_deref(),
                )?;
                print_current_status_table(&statuses);
            }
        },
        Commands::Thumbnail { command } => match command {
            ThumbnailCommands::Set {
                store,
                server,
                auth_token,
                profile,
                category,
                asset_code,
                department,
                version,
                image,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let record = if let Some(server) = server {
                    let auth_token = resolve_remote_auth_token(auth_token.as_deref())?;
                    let remote = RemoteClient::new(&server, &auth_token)?;
                    let profile = profile.as_deref().unwrap_or("default");
                    set_remote_thumbnail(&remote, profile, &department_key, version, &image)?
                } else {
                    let store = store
                        .as_deref()
                        .ok_or_else(|| anyhow!("--store is required unless --server is used"))?;
                    let store = Store::open(store)?;
                    store.set_thumbnail(&department_key, version, &image)?
                };
                println!(
                    "thumbnail set {} {}/{}/{} sha256={} mime={} size={}",
                    record.version,
                    record.department_key.asset_key.category,
                    record.department_key.asset_key.asset_code,
                    record.department_key.department,
                    record.sha256,
                    record.mime_type,
                    record.size
                );
            }
            ThumbnailCommands::Get {
                store,
                category,
                asset_code,
                department,
                version,
                latest,
                force,
                dest,
            } => {
                if latest && version.is_some() {
                    bail!("--latest and --version cannot be used together");
                }
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let selector = if latest {
                    VersionSelector::Latest
                } else {
                    version.map_or(VersionSelector::Current, VersionSelector::Version)
                };
                let store = Store::open_read_only(&store)?;
                let record = store.copy_thumbnail(&department_key, selector, &dest, force)?;
                println!(
                    "thumbnail copied {} {}/{}/{} to {}",
                    record.version,
                    record.department_key.asset_key.category,
                    record.department_key.asset_key.asset_code,
                    record.department_key.department,
                    dest.display()
                );
            }
            ThumbnailCommands::Info {
                store,
                category,
                asset_code,
                department,
                version,
                latest,
            } => {
                if latest && version.is_some() {
                    bail!("--latest and --version cannot be used together");
                }
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let selector = if latest {
                    VersionSelector::Latest
                } else {
                    version.map_or(VersionSelector::Current, VersionSelector::Version)
                };
                let store = Store::open_read_only(&store)?;
                let record = store.thumbnail_info(&department_key, selector)?;
                println!("{}", serde_json::to_string_pretty(&record)?);
            }
            ThumbnailCommands::List {
                store,
                category,
                asset_code,
                department,
            } => {
                if let Some(category) = &category {
                    validate_category(category)?;
                }
                if let Some(asset_code) = &asset_code {
                    validate_asset_code(asset_code)?;
                }
                if let Some(department) = &department {
                    validate_department(department)?;
                }
                let store = Store::open_read_only(&store)?;
                let records = store.list_thumbnails(
                    category.as_deref(),
                    asset_code.as_deref(),
                    department.as_deref(),
                )?;
                print_thumbnail_table(&records);
            }
            ThumbnailCommands::Url {
                store,
                category,
                asset_code,
                department,
                version,
                latest,
                remote_base_url,
            } => {
                if latest && version.is_some() {
                    bail!("--latest and --version cannot be used together");
                }
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let selector = if latest {
                    VersionSelector::Latest
                } else {
                    version.map_or(VersionSelector::Current, VersionSelector::Version)
                };
                let store = Store::open_read_only(&store)?;
                let url =
                    store.thumbnail_url(&department_key, selector, remote_base_url.as_deref())?;
                println!("{url}");
            }
            ThumbnailCommands::Remove {
                store,
                category,
                asset_code,
                department,
                version,
                latest,
            } => {
                if latest && version.is_some() {
                    bail!("--latest and --version cannot be used together");
                }
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let selector = if latest {
                    VersionSelector::Latest
                } else {
                    version.map_or(VersionSelector::Current, VersionSelector::Version)
                };
                let store = Store::open(&store)?;
                let removed = store.remove_thumbnail(&department_key, selector)?;
                println!(
                    "thumbnail removed {} {}/{}/{}",
                    removed.version,
                    removed.department_key.asset_key.category,
                    removed.department_key.asset_key.asset_code,
                    removed.department_key.department
                );
            }
        },
        Commands::List {
            store,
            category,
            asset_code,
            department,
        } => {
            if let Some(category) = &category {
                validate_category(category)?;
            }
            if let Some(asset_code) = &asset_code {
                validate_asset_code(asset_code)?;
            }
            if let Some(department) = &department {
                validate_department(department)?;
            }

            let store = Store::open_read_only(&store)?;
            let versions = store.list_versions(
                category.as_deref(),
                asset_code.as_deref(),
                department.as_deref(),
            )?;
            let statuses = store.current_status(
                category.as_deref(),
                asset_code.as_deref(),
                department.as_deref(),
            )?;
            let current_versions = current_versions_by_department(&statuses);
            print_version_table(&versions, &current_versions);
        }
        Commands::Info {
            store,
            category,
            asset_code,
            department,
            version,
        } => {
            let asset_key = AssetKey::new(category, asset_code)?;
            let store = Store::open_read_only(&store)?;
            match (department, version) {
                (Some(department), version) => {
                    let department_key = DepartmentKey::new(asset_key, department)?;
                    let selector =
                        version.map_or(VersionSelector::Current, VersionSelector::Version);
                    let info = store.version_info_by_selector(&department_key, selector)?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
                (None, None) => {
                    let info = store.asset_info(&asset_key)?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
                (None, Some(_)) => bail!("--department is required when --version is used"),
            }
        }
        Commands::Checkout {
            store,
            category,
            asset_code,
            department,
            version,
            latest,
            force,
            dest,
        } => {
            if latest && version.is_some() {
                bail!("--latest and --version cannot be used together");
            }
            let asset_key = AssetKey::new(category, asset_code)?;
            let department_key = DepartmentKey::new(asset_key, department)?;
            let store = Store::open_read_only(&store)?;
            let selector = if latest {
                VersionSelector::Latest
            } else {
                version.map_or(VersionSelector::Current, VersionSelector::Version)
            };
            let record = store.checkout(&department_key, selector, &dest, force)?;
            println!(
                "checked out {} {}/{}/{} to {}",
                record.version,
                record.department_key.asset_key.category,
                record.department_key.asset_key.asset_code,
                record.department_key.department,
                dest.display()
            );
        }
        Commands::Fetch(args) => {
            if args.latest && args.version.is_some() {
                bail!("--latest and --version cannot be used together");
            }
            if args.materialize && args.workspace.is_none() {
                bail!("--workspace is required with --materialize");
            }
            let store = Store::open_or_init(&args.store)?;
            let remote = RemoteClient::new(&args.server, &args.auth_token)?;
            let selector = if args.latest {
                VersionSelector::Latest
            } else {
                args.version
                    .map_or(VersionSelector::Current, VersionSelector::Version)
            };
            let (version_info, stats) = fetch_remote_version(
                &store,
                &remote,
                &args.profile,
                &args.category,
                &args.asset_code,
                &args.department,
                selector,
            )?;
            let materialized = if args.materialize {
                let workspace = workspace_root(args.workspace)?;
                Some(store.materialize(
                    &workspace,
                    &version_info.version.department_key,
                    VersionSelector::Version(version_info.version.version),
                    args.force,
                )?)
            } else {
                None
            };
            if selector == VersionSelector::Current {
                let status = remote.fetch_current_status(
                    &args.profile,
                    &args.category,
                    &args.asset_code,
                    &args.department,
                )?;
                apply_remote_current_status(&store, &status)?;
            }
            println!(
                "fetched {} {}/{}/{} objects_downloaded={} objects_reused={} bytes_downloaded={}",
                version_info.version.version,
                version_info.version.department_key.asset_key.category,
                version_info.version.department_key.asset_key.asset_code,
                version_info.version.department_key.department,
                stats.objects_downloaded,
                stats.objects_reused,
                stats.bytes_downloaded
            );
            if let Some(materialized) = materialized {
                println!(
                    "materialized {} to {}",
                    materialized.version,
                    materialized.path.display()
                );
            }
        }
        Commands::Sync(args) => {
            if args.latest && args.all_versions {
                bail!("--latest and --all-versions cannot be used together");
            }
            if args.materialize && args.all_versions {
                bail!("--materialize cannot be combined with --all-versions");
            }
            if args.materialize && args.workspace.is_none() {
                bail!("--workspace is required with --materialize");
            }
            if let Some(category) = &args.category {
                validate_category(category)?;
            }
            if let Some(asset_code) = &args.asset_code {
                validate_asset_code(asset_code)?;
            }
            if let Some(department) = &args.department {
                validate_department(department)?;
            }

            let store = Store::open_or_init(&args.store)?;
            let remote = RemoteClient::new(&args.server, &args.auth_token)?;
            let assets = remote.fetch_assets(
                &args.profile,
                args.category.as_deref(),
                args.asset_code.as_deref(),
                args.department.as_deref(),
            )?;
            let workspace = if args.materialize {
                Some(workspace_root(args.workspace.clone())?)
            } else {
                None
            };
            let mut stats = SyncStats {
                assets_seen: assets.assets.len() as u64,
                ..SyncStats::default()
            };
            for asset in assets.assets {
                if args.all_versions {
                    let versions = remote.fetch_versions(
                        &args.profile,
                        &asset.category,
                        &asset.asset_code,
                        &asset.department,
                    )?;
                    let current_status = versions.current_status.clone();
                    for record in versions.versions {
                        let (_info, fetched) = fetch_remote_version(
                            &store,
                            &remote,
                            &args.profile,
                            &asset.category,
                            &asset.asset_code,
                            &asset.department,
                            VersionSelector::Version(record.version),
                        )?;
                        stats.add_fetch(fetched);
                        stats.versions_synced += 1;
                    }
                    apply_remote_current_status(&store, &current_status)?;
                    continue;
                }

                let version = if args.latest {
                    asset.latest
                } else {
                    asset.current
                };
                let Some(version) = version else {
                    continue;
                };
                let (info, fetched) = fetch_remote_version(
                    &store,
                    &remote,
                    &args.profile,
                    &asset.category,
                    &asset.asset_code,
                    &asset.department,
                    VersionSelector::Version(version),
                )?;
                stats.add_fetch(fetched);
                stats.versions_synced += 1;
                if !args.latest {
                    let current_status = CurrentStatus {
                        department_key: info.version.department_key.clone(),
                        current: asset.current,
                        latest: asset.latest,
                        explicit: asset.explicit_current,
                    };
                    apply_remote_current_status(&store, &current_status)?;
                }
                if let Some(workspace) = &workspace {
                    store.materialize(
                        workspace,
                        &info.version.department_key,
                        VersionSelector::Version(info.version.version),
                        args.force,
                    )?;
                    stats.materialized += 1;
                }
            }
            println!(
                "synced assets={} versions={} objects_downloaded={} objects_reused={} bytes_downloaded={} materialized={}",
                stats.assets_seen,
                stats.versions_synced,
                stats.objects_downloaded,
                stats.objects_reused,
                stats.bytes_downloaded,
                stats.materialized
            );
        }
        Commands::Push(args) => {
            if args.latest && args.version.is_some() {
                bail!("--latest and --version cannot be used together");
            }
            let asset_key = AssetKey::new(args.category, args.asset_code)?;
            let department_key = DepartmentKey::new(asset_key, args.department)?;
            let selector = if args.latest {
                VersionSelector::Latest
            } else {
                args.version
                    .map_or(VersionSelector::Current, VersionSelector::Version)
            };
            let store = Store::open_read_only(&args.store)?;
            let remote = RemoteClient::new(&args.server, &args.auth_token)?;
            let (version_info, stats) =
                push_remote_version(&store, &remote, &args.profile, &department_key, selector)?;
            if args.set_current {
                remote.set_current_version(
                    &args.profile,
                    &version_info.version.department_key,
                    version_info.version.version,
                )?;
            } else if selector == VersionSelector::Current {
                let status = store.current_status_for_department(&department_key)?;
                remote.apply_current_status(&args.profile, &status)?;
            }
            println!(
                "pushed {} {}/{}/{} objects_uploaded={} objects_reused={} bytes_uploaded={} thumbnails_pushed={}",
                version_info.version.version,
                version_info.version.department_key.asset_key.category,
                version_info.version.department_key.asset_key.asset_code,
                version_info.version.department_key.department,
                stats.objects_uploaded,
                stats.objects_reused,
                stats.bytes_uploaded,
                stats.thumbnails_pushed
            );
        }
        Commands::Materialize {
            store,
            workspace,
            category,
            asset_code,
            department,
            version,
            latest,
            force,
        } => {
            if latest && version.is_some() {
                bail!("--latest and --version cannot be used together");
            }
            let asset_key = AssetKey::new(category, asset_code)?;
            let department_key = DepartmentKey::new(asset_key, department)?;
            let workspace = workspace_root(workspace)?;
            let store = Store::open_read_only(&store)?;
            let selector = if latest {
                VersionSelector::Latest
            } else {
                version.map_or(VersionSelector::Current, VersionSelector::Version)
            };
            print_workspace_restore_outcome(
                &department_key,
                store.materialize(&workspace, &department_key, selector, force)?,
                WorkspaceRestoreWords::new("materialized", "already materialized"),
            );
        }
        Commands::Resolve {
            store,
            workspace,
            mode,
            remote_base_url,
            asset_path,
        } => {
            let workspace = workspace_root(workspace)?;
            let asset_path = AssetPath::parse(&asset_path)?;
            let store = Store::open_read_only(&store)?;
            let outcome = store.resolve_asset_path(
                &workspace,
                &asset_path,
                mode,
                remote_base_url.as_deref(),
            )?;
            println!("{}", outcome.location);
        }
        Commands::SetRemote {
            store,
            remote_base_url,
        } => {
            let store = Store::open(&store)?;
            let remote_base_url = store.set_remote_base_url(&remote_base_url)?;
            println!("remote base URL set to {remote_base_url}");
        }
        Commands::Serve {
            bind,
            auth_token,
            profiles,
            store,
            workspace,
            max_upload_mb,
            max_object_upload_mb,
            cors_allow_origins,
        } => {
            let config = ServeConfig::from_args(
                bind,
                auth_token,
                profiles,
                store,
                workspace,
                max_upload_mb,
                max_object_upload_mb,
                cors_allow_origins,
            )?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build Tokio runtime")?;
            runtime.block_on(serve_web(config))?;
        }
        Commands::Publish { command } => match command {
            PublishCommands::Promote {
                store,
                category,
                asset_code,
                department,
                wip_seq,
                no_validate,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open(&store)?;
                let wip = match wip_seq {
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
                if !no_validate {
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
                    print_publish_validate_report(&report, "promote validation gate")?;
                }
                let outcome = store.promote_wip(&department_key, Some(wip.seq))?;
                println!(
                    "{} {} {}/{}/{} files={} bytes={} manifest={}",
                    if outcome.created {
                        "promoted"
                    } else {
                        "reused"
                    },
                    outcome.version,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    outcome.file_count,
                    outcome.total_bytes,
                    outcome.manifest_hash
                );
            }
            PublishCommands::Validate {
                store,
                category,
                asset_code,
                department,
                version,
                latest,
                wip,
                wip_seq,
                source,
            } => {
                let report = if let Some(source) = source {
                    if store.is_some() || wip || wip_seq.is_some() || version.is_some() || latest {
                        bail!("--source cannot be combined with store target options");
                    }
                    validate_source_references(&source)?
                } else {
                    let store_path = store
                        .ok_or_else(|| anyhow!("--store is required unless --source is used"))?;
                    let (Some(category), Some(asset_code), Some(department)) =
                        (category, asset_code, department)
                    else {
                        bail!(
                            "--category, --asset-code, and --department are required unless --source is used"
                        );
                    };
                    let asset_key = AssetKey::new(category, asset_code)?;
                    let department_key = DepartmentKey::new(asset_key, department)?;
                    let store = Store::open_read_only(&store_path)?;
                    let (target, manifest_hash) = if wip || wip_seq.is_some() {
                        let record = match wip_seq {
                            Some(seq) => store.get_wip(&department_key, seq)?,
                            None => store.wip_head(&department_key)?.ok_or_else(|| {
                                anyhow!(
                                    "department has no wip versions: {}/{}/{}",
                                    department_key.asset_key.category,
                                    department_key.asset_key.asset_code,
                                    department_key.department
                                )
                            })?,
                        };
                        (format!("wip seq {}", record.seq), record.manifest_hash)
                    } else {
                        let selector = if latest {
                            VersionSelector::Latest
                        } else {
                            version.map_or(VersionSelector::Current, VersionSelector::Version)
                        };
                        let resolved = store
                            .selected_version(&department_key, selector)?
                            .ok_or_else(|| {
                                anyhow!(
                                    "department has no selected version: {}/{}/{}",
                                    department_key.asset_key.category,
                                    department_key.asset_key.asset_code,
                                    department_key.department
                                )
                            })?;
                        let record = store.get_version(&department_key, resolved)?;
                        (format!("version {resolved}"), record.manifest_hash)
                    };
                    let manifest = store.get_manifest(&manifest_hash)?;
                    validate_manifest_references(&store, target, &manifest)?
                };
                print_publish_validate_report(&report, "publish validation")?;
                println!(
                    "ok files_scanned={} references_checked={} warnings={}",
                    report.files_scanned,
                    report.references_checked,
                    report.warnings.len()
                );
            }
        },
        Commands::Verify { store } => {
            let store = Store::open_read_only(&store)?;
            let report = store.verify()?;
            if report.errors.is_empty() {
                println!(
                    "ok manifests={} versions={} thumbnails={} objects_checked={}",
                    report.manifest_count,
                    report.version_count,
                    report.thumbnail_count,
                    report.objects_checked
                );
            } else {
                for error in &report.errors {
                    eprintln!("verify error: {error}");
                }
                bail!(
                    "verification failed: {} error(s), manifests={}, versions={}, thumbnails={}, objects_checked={}",
                    report.errors.len(),
                    report.manifest_count,
                    report.version_count,
                    report.thumbnail_count,
                    report.objects_checked
                );
            }
        }
    }
    Ok(())
}

/// Prints validation warnings to stderr and fails when the report carries
/// errors.
fn print_publish_validate_report(report: &PublishValidateReport, context: &str) -> Result<()> {
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    if !report.errors.is_empty() {
        for error in &report.errors {
            eprintln!("error: {error}");
        }
        bail!(
            "{context} failed for {}: {} error(s), files_scanned={}, references_checked={}",
            report.target,
            report.errors.len(),
            report.files_scanned,
            report.references_checked
        );
    }
    Ok(())
}

fn resolve_remote_auth_token(cli_token: Option<&str>) -> Result<String> {
    cli_token
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("ADS_WEB_TOKEN")
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
        .ok_or_else(|| anyhow!("--auth-token or ADS_WEB_TOKEN is required with --server"))
}

/// Validates the publish reference policy (schema v8) over a manifest stored
/// in the content-addressed store. Cross-asset references must use ads://;
/// intra-version references may be relative as long as they resolve to
/// another file of the same manifest, because the manifest view preserves the
/// relative layout. Absolute paths, file:// URIs, and references escaping or
/// missing from the version are errors. Binary USD layers cannot be scanned
/// and produce warnings.
fn validate_manifest_references(
    store: &Store,
    target: String,
    manifest: &Manifest,
) -> Result<PublishValidateReport> {
    let mut report = PublishValidateReport {
        target,
        files_scanned: 0,
        references_checked: 0,
        warnings: Vec::new(),
        errors: Vec::new(),
    };
    let entry_paths: BTreeSet<String> = manifest
        .entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect();

    for entry in &manifest.entries {
        if !is_usd_layer_path(Path::new(&entry.relative_path)) {
            continue;
        }
        report.files_scanned += 1;
        let path = object_path(&store.root, &entry.sha256);
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "failed to read object for {}: {}",
                entry.relative_path,
                path.display()
            )
        })?;
        scan_usd_text(&mut report, &entry_paths, &entry.relative_path, bytes);
    }

    Ok(report)
}

/// Validates the same publish reference policy over an arbitrary source
/// folder, before registration.
fn validate_source_references(source: &Path) -> Result<PublishValidateReport> {
    let root = source
        .canonicalize()
        .with_context(|| format!("source folder does not exist: {}", source.display()))?;
    if !root.is_dir() {
        bail!("source path is not a folder: {}", root.display());
    }

    let mut report = PublishValidateReport {
        target: root.display().to_string(),
        files_scanned: 0,
        references_checked: 0,
        warnings: Vec::new(),
        errors: Vec::new(),
    };

    let mut entry_paths = BTreeSet::new();
    let mut usd_files = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        if entry.path() == root || entry.file_type().is_dir() {
            continue;
        }
        let rel_path = entry
            .path()
            .strip_prefix(&root)
            .with_context(|| format!("failed to relativize {}", entry.path().display()))?;
        let rel_path = normalize_relative_path(rel_path)?;
        if entry.file_type().is_symlink() {
            report.errors.push(format!(
                "{rel_path} is a symlink; publish does not allow symlinks"
            ));
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if is_usd_layer_path(entry.path()) {
            usd_files.push((rel_path.clone(), entry.path().to_path_buf()));
        }
        entry_paths.insert(rel_path);
    }

    for (rel_path, path) in usd_files {
        report.files_scanned += 1;
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        scan_usd_text(&mut report, &entry_paths, &rel_path, bytes);
    }

    Ok(report)
}

fn scan_usd_text(
    report: &mut PublishValidateReport,
    entry_paths: &BTreeSet<String>,
    rel_path: &str,
    bytes: Vec<u8>,
) {
    let Ok(text) = String::from_utf8(bytes) else {
        report.warnings.push(format!(
            "{rel_path} is not UTF-8 text; binary USD reference validation was skipped"
        ));
        return;
    };
    for reference in extract_usd_asset_references(&text) {
        report.references_checked += 1;
        validate_publish_reference(report, entry_paths, rel_path, &reference);
    }
}

fn validate_publish_reference(
    report: &mut PublishValidateReport,
    entry_paths: &BTreeSet<String>,
    rel_path: &str,
    reference: &str,
) {
    let reference = reference.trim();
    if reference.is_empty() || reference.starts_with("ads://") {
        return;
    }
    if looks_like_external_uri(reference) && !reference.starts_with("file://") {
        report.warnings.push(format!(
            "{rel_path} contains external URI reference `{reference}`"
        ));
        return;
    }
    if reference.starts_with("file://") {
        report.errors.push(format!(
            "{rel_path} contains unmanaged file URI reference `{reference}`; expected ads://"
        ));
        return;
    }
    if is_absolute_asset_path(reference) {
        report.errors.push(format!(
            "{rel_path} contains unmanaged absolute path reference `{reference}`; expected ads://"
        ));
        return;
    }
    match resolve_version_relative_reference(rel_path, reference) {
        Some(resolved) if entry_paths.contains(&resolved) => {}
        Some(resolved) => report.errors.push(format!(
            "{rel_path} references `{reference}`, which is missing from the version (resolved to `{resolved}`)"
        )),
        None => report.errors.push(format!(
            "{rel_path} references `{reference}`, which escapes the version root"
        )),
    }
}

/// Lexically resolves a relative USD reference against the referencing
/// layer's directory within the version. Returns None when the reference
/// escapes the version root.
fn resolve_version_relative_reference(from: &str, reference: &str) -> Option<String> {
    let mut parts: Vec<String> = from.split('/').map(str::to_string).collect();
    parts.pop();
    let normalized = reference.replace('\\', "/");
    for component in normalized.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other.to_string()),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn extract_usd_asset_references(text: &str) -> Vec<String> {
    // USD wraps asset paths that themselves contain `@` in triple-`@`
    // delimiters, so `@@@` must be matched before plain `@` pairs — a
    // naive per-`@` toggle misparses everything after the first escape.
    let mut references = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find('@') {
        let after = &remaining[start..];
        let (delimiter, body) = match after.strip_prefix("@@@") {
            Some(body) => ("@@@", body),
            None => ("@", &after[1..]),
        };
        match body.find(delimiter) {
            Some(end) => {
                references.push(body[..end].to_string());
                remaining = &body[end + delimiter.len()..];
            }
            // Unterminated reference; nothing after it can parse.
            None => break,
        }
    }
    references
}

fn is_usd_layer_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| USD_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn looks_like_external_uri(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && !rest.is_empty()
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

fn is_absolute_asset_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || (value.len() >= 3
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'/' | b'\\')
            && value.as_bytes()[0].is_ascii_alphabetic())
}

#[derive(Clone, Copy)]
struct WorkspaceRestoreWords {
    done: &'static str,
    unchanged: &'static str,
}

impl WorkspaceRestoreWords {
    const fn new(done: &'static str, unchanged: &'static str) -> Self {
        Self { done, unchanged }
    }
}

fn print_workspace_restore_outcome(
    department_key: &DepartmentKey,
    outcome: MaterializeOutcome,
    words: WorkspaceRestoreWords,
) {
    if outcome.unchanged {
        println!(
            "{} {} {}/{}/{} at {}",
            words.unchanged,
            outcome.version,
            department_key.asset_key.category,
            department_key.asset_key.asset_code,
            department_key.department,
            outcome.path.display()
        );
    } else {
        println!(
            "{} {} {}/{}/{} to {}",
            words.done,
            outcome.version,
            department_key.asset_key.category,
            department_key.asset_key.asset_code,
            department_key.department,
            outcome.path.display()
        );
    }
}

fn fetch_remote_version(
    store: &Store,
    remote: &RemoteClient,
    profile: &str,
    category: &str,
    asset_code: &str,
    department: &str,
    selector: VersionSelector,
) -> Result<(VersionInfo, FetchVersionStats)> {
    let version_info =
        remote.fetch_version_info(profile, category, asset_code, department, selector)?;
    let mut stats = FetchVersionStats::default();
    for entry in &version_info.manifest.entries {
        if store.object_is_valid(&entry.sha256, entry.size)? {
            stats.objects_reused += 1;
            continue;
        }
        stats.bytes_downloaded += remote.fetch_object(profile, &entry.sha256, store)?;
        stats.objects_downloaded += 1;
    }
    store.import_version_info(&version_info)?;
    Ok((version_info, stats))
}

fn apply_remote_current_status(store: &Store, status: &CurrentStatus) -> Result<()> {
    if status.explicit {
        let version = status.current.ok_or_else(|| {
            anyhow!(
                "remote current status is explicit but has no current version: {}/{}/{}",
                status.department_key.asset_key.category,
                status.department_key.asset_key.asset_code,
                status.department_key.department
            )
        })?;
        store.set_current_version(&status.department_key, version)?;
    } else {
        store.reset_current_version(&status.department_key)?;
    }
    Ok(())
}

fn push_remote_version(
    store: &Store,
    remote: &RemoteClient,
    profile: &str,
    department_key: &DepartmentKey,
    selector: VersionSelector,
) -> Result<(VersionInfo, PushVersionStats)> {
    let version_info = store.version_info_by_selector(department_key, selector)?;
    let mut stats = PushVersionStats::default();
    for entry in &version_info.manifest.entries {
        if !store.object_is_valid(&entry.sha256, entry.size)? {
            bail!(
                "local object missing or invalid for {}: {}",
                entry.relative_path,
                entry.sha256
            );
        }
        if remote
            .object_status(profile, &entry.sha256, entry.size)?
            .exists
        {
            stats.objects_reused += 1;
            continue;
        }
        let upload = remote.upload_object(
            profile,
            &entry.sha256,
            &object_path(&store.root, &entry.sha256),
        )?;
        if upload.reused {
            stats.objects_reused += 1;
        } else {
            stats.objects_uploaded += 1;
            stats.bytes_uploaded += upload.size;
        }
    }
    remote.import_version_info(profile, &version_info)?;
    if let Some(thumbnail) =
        store.try_get_thumbnail(department_key, version_info.version.version)?
    {
        if !store.object_is_valid(&thumbnail.sha256, thumbnail.size)? {
            bail!(
                "local thumbnail object missing or invalid for {}/{}/{} {}: {}",
                thumbnail.department_key.asset_key.category,
                thumbnail.department_key.asset_key.asset_code,
                thumbnail.department_key.department,
                thumbnail.version,
                thumbnail.sha256
            );
        }
        if remote
            .object_status(profile, &thumbnail.sha256, thumbnail.size)?
            .exists
        {
            stats.objects_reused += 1;
        } else {
            let upload = remote.upload_object(
                profile,
                &thumbnail.sha256,
                &object_path(&store.root, &thumbnail.sha256),
            )?;
            if upload.reused {
                stats.objects_reused += 1;
            } else {
                stats.objects_uploaded += 1;
                stats.bytes_uploaded += upload.size;
            }
        }
        remote.import_thumbnail_info(profile, &thumbnail)?;
        stats.thumbnails_pushed += 1;
    }
    Ok((version_info, stats))
}

fn add_remote_source(
    remote: &RemoteClient,
    profile: &str,
    source: Option<&Path>,
    department_key: &DepartmentKey,
    version: Option<VersionId>,
) -> Result<AddOutcome> {
    let source = source.ok_or_else(|| anyhow!("--source is required with --server"))?;
    let source_files = scan_source_for_upload(source)?;
    let manifest = Manifest {
        entries: source_files
            .iter()
            .map(|source_file| source_file.entry.clone())
            .collect(),
    };
    let manifest_hash = manifest.canonical_hash()?;
    let versions = remote.fetch_versions(
        profile,
        &department_key.asset_key.category,
        &department_key.asset_key.asset_code,
        &department_key.department,
    )?;
    if let Some(record) = version.and_then(|version| {
        versions
            .versions
            .iter()
            .find(|record| record.version == version)
    }) {
        if record.manifest_hash == manifest_hash {
            return Ok(AddOutcome {
                created: false,
                version: record.version,
                manifest_hash,
                file_count: record.file_count,
                total_bytes: record.total_bytes,
            });
        }
        bail!(
            "version already exists with different content: {}/{}/{} {}",
            department_key.asset_key.category,
            department_key.asset_key.asset_code,
            department_key.department,
            record.version
        );
    }
    if let Some(record) = versions
        .versions
        .iter()
        .find(|record| record.manifest_hash == manifest_hash)
    {
        return Ok(AddOutcome {
            created: false,
            version: record.version,
            manifest_hash,
            file_count: record.file_count,
            total_bytes: record.total_bytes,
        });
    }
    let latest = versions
        .current_status
        .latest
        .or_else(|| versions.versions.iter().map(|record| record.version).max());
    let version = match version {
        Some(version) => version,
        None => latest.map_or(VersionId(1), VersionId::next),
    };
    let expected_version = latest.map_or(VersionId(1), VersionId::next);
    if version != expected_version {
        bail!(
            "version must be the next version for {}/{}/{}: expected {}, got {}",
            department_key.asset_key.category,
            department_key.asset_key.asset_code,
            department_key.department,
            expected_version,
            version
        );
    }

    let mut uploaded = BTreeSet::new();
    for source_file in &source_files {
        if !uploaded.insert(source_file.entry.sha256.clone()) {
            continue;
        }
        let status =
            remote.object_status(profile, &source_file.entry.sha256, source_file.entry.size)?;
        if status.exists {
            continue;
        }
        remote.upload_object(profile, &source_file.entry.sha256, &source_file.path)?;
    }

    let version_info = VersionInfo {
        version: VersionRecord {
            department_key: department_key.clone(),
            version,
            manifest_hash: manifest_hash.clone(),
            created_at: Utc::now().to_rfc3339(),
            source_path: source.display().to_string(),
            file_count: manifest.entries.len() as u64,
            total_bytes: manifest.total_bytes(),
            promoted_from: None,
        },
        manifest,
    };
    remote.import_version_info(profile, &version_info)?;
    Ok(AddOutcome {
        created: true,
        version,
        manifest_hash,
        file_count: version_info.version.file_count,
        total_bytes: version_info.version.total_bytes,
    })
}

fn set_remote_thumbnail(
    remote: &RemoteClient,
    profile: &str,
    department_key: &DepartmentKey,
    version: VersionId,
    image: &Path,
) -> Result<ThumbnailRecord> {
    let image = image
        .canonicalize()
        .with_context(|| format!("thumbnail image does not exist: {}", image.display()))?;
    if !image.is_file() {
        bail!("thumbnail image is not a file: {}", image.display());
    }
    let image_info = inspect_thumbnail_image(&image)?;
    let (sha256, size) = hash_file(&image)?;
    if !remote.object_status(profile, &sha256, size)?.exists {
        remote.upload_object(profile, &sha256, &image)?;
    }
    let record = ThumbnailRecord {
        department_key: department_key.clone(),
        version,
        sha256,
        size,
        mime_type: image_info.mime_type,
        width: image_info.width,
        height: image_info.height,
        created_at: Utc::now().to_rfc3339(),
        source_path: image.display().to_string(),
    };
    remote.import_thumbnail_info(profile, &record)
}

#[derive(Clone, Debug)]
struct SourceUploadFile {
    entry: ManifestEntry,
    path: PathBuf,
}

fn scan_source_for_upload(source: &Path) -> Result<Vec<SourceUploadFile>> {
    let source_abs = source
        .canonicalize()
        .with_context(|| format!("source folder does not exist: {}", source.display()))?;
    if !source_abs.is_dir() {
        bail!("source is not a folder: {}", source.display());
    }

    let ignore_rules = IgnoreRules::load(&source_abs)?;
    let mut files = Vec::new();
    for entry in WalkDir::new(&source_abs)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend_upload_entry(entry, &source_abs, &ignore_rules))
    {
        let entry = entry.with_context(|| format!("failed to walk {}", source_abs.display()))?;
        if entry.path() == source_abs {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        if file_type.is_symlink() {
            bail!("symlinks are not supported: {}", entry.path().display());
        }
        if !file_type.is_file() {
            continue;
        }

        let rel_path = entry
            .path()
            .strip_prefix(&source_abs)
            .with_context(|| format!("failed to relativize {}", entry.path().display()))?;
        if is_default_ignored(rel_path, false) || ignore_rules.is_ignored(rel_path, false) {
            continue;
        }
        let relative_path = normalize_relative_path(rel_path)?;
        let (sha256, size) = hash_file(entry.path())?;
        files.push(SourceUploadFile {
            entry: ManifestEntry {
                relative_path,
                sha256,
                size,
                mode: simple_mode(entry.path())?,
            },
            path: entry.path().to_path_buf(),
        });
    }
    files.sort_by(|left, right| left.entry.relative_path.cmp(&right.entry.relative_path));
    Ok(files)
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct AssetKey {
    pub category: String,
    pub asset_code: String,
}

impl AssetKey {
    pub fn new(category: String, asset_code: String) -> Result<Self> {
        validate_category(&category)?;
        validate_asset_code(&asset_code)?;
        Ok(Self {
            category,
            asset_code,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct DepartmentKey {
    pub asset_key: AssetKey,
    pub department: String,
}

impl DepartmentKey {
    pub fn new(asset_key: AssetKey, department: String) -> Result<Self> {
        validate_department(&department)?;
        Ok(Self {
            asset_key,
            department,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VersionId(pub u32);

impl VersionId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Fixed-width encoding for RocksDB keys so lexicographic order matches
    /// numeric order (the display form `v###` is variable width and breaks
    /// ordering past v999).
    fn key_encode(self) -> String {
        format!("{:010}", self.0)
    }
}

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{:03}", self.0)
    }
}

impl FromStr for VersionId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        // Lenient parse is a permanent contract: pinned URIs like `?v=v003`
        // are baked into published USD files, while the canonical form is a
        // bare integer.
        let digits = value.strip_prefix('v').unwrap_or(value);
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            bail!("version must be an integer like 12 or v012: {value}");
        }
        let number = digits
            .parse::<u32>()
            .with_context(|| format!("invalid version number: {value}"))?;
        if number == 0 {
            bail!("version must be greater than zero: {value}");
        }
        Ok(Self(number))
    }
}

impl Serialize for VersionId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for VersionId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct VersionVisitor;

        impl Visitor<'_> for VersionVisitor {
            type Value = VersionId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a version number like 12 or a version string like v012")
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == 0 || value > u64::from(u32::MAX) {
                    return Err(E::custom(format!("version out of range: {value}")));
                }
                Ok(VersionId(value as u32))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = u64::try_from(value)
                    .map_err(|_| E::custom(format!("version out of range: {value}")))?;
                self.visit_u64(value)
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                VersionId::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(VersionVisitor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ResolveMode {
    Auto,
    Local,
    Remote,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPath {
    pub parts: Vec<String>,
    pub version: VersionSelector,
    pub version_explicit: bool,
}

impl AssetPath {
    pub fn parse(value: &str) -> Result<Self> {
        let logical = value.strip_prefix("ads://").unwrap_or(value);
        if logical.starts_with("ads:") {
            bail!(
                "asset path must use ads://asset_code/department/path or ads://category/.../asset_code/department/path: {value}"
            );
        }
        let (logical, query) = logical
            .split_once('?')
            .map_or((logical, None), |(path, query)| (path, Some(query)));
        let mut version = VersionSelector::Current;
        let mut version_explicit = false;
        if let Some(query) = query {
            for pair in query.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow!("invalid asset path query parameter: {pair}"))?;
                if key != "v" {
                    bail!("unsupported asset path query parameter: {key}");
                }
                if version_explicit {
                    bail!("asset path query parameter v must only be specified once");
                }
                version = VersionSelector::parse(value)
                    .ok_or_else(|| anyhow!("invalid version selector in asset path: {value}"))?;
                version_explicit = true;
            }
        }

        let logical = logical.trim_start_matches('/');
        let parts = logical.split('/').collect::<Vec<_>>();
        if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
            bail!(
                "asset path must be asset_code/department[/path] or category/.../asset_code/department[/path]: {value}"
            );
        }

        Ok(Self {
            parts: parts.into_iter().map(str::to_string).collect(),
            version,
            version_explicit,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionSelector {
    Current,
    Latest,
    /// Head of the local WIP micro-version stream (schema v8). Local-only:
    /// never pushed, never cached by resolvers.
    Wip,
    Version(VersionId),
}

impl VersionSelector {
    fn parse(value: &str) -> Option<Self> {
        if value == "current" {
            return Some(Self::Current);
        }
        if value == "latest" {
            return Some(Self::Latest);
        }
        if value == "wip" {
            return Some(Self::Wip);
        }
        VersionId::from_str(value).ok().map(Self::Version)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolveSource {
    Local,
    Cache,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssetFileKind {
    /// May reference sibling files relatively; resolves through the manifest
    /// view so those references keep working.
    Composing,
    /// Pure leaf content; resolves lazily to the flat blob cache.
    Leaf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolveOutcome {
    pub location: String,
    pub source: ResolveSource,
    /// None for WIP resolutions: micro-versions carry no publish number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionId>,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetRecord {
    pub asset_key: AssetKey,
    pub created_at: String,
    pub latest_versions: BTreeMap<String, VersionId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionRecord {
    pub department_key: DepartmentKey,
    pub version: VersionId,
    pub manifest_hash: String,
    pub created_at: String,
    pub source_path: String,
    pub file_count: u64,
    pub total_bytes: u64,
    /// WIP sequence this version was promoted from, when published via
    /// `publish promote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<ManifestEntry>,
}

impl SyncStats {
    fn add_fetch(&mut self, stats: FetchVersionStats) {
        self.objects_downloaded += stats.objects_downloaded;
        self.objects_reused += stats.objects_reused;
        self.bytes_downloaded += stats.bytes_downloaded;
    }
}

impl Manifest {
    pub fn canonical_hash(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)?;
        Ok(sha256_bytes(&bytes))
    }

    /// Rejects manifests that are not in the canonical form `build_manifest`
    /// produces: entries strictly ascending by relative_path. The canonical
    /// hash is order-sensitive, so an unsorted manifest for identical content
    /// would hash differently and silently defeat wip-head/version dedup, and
    /// a duplicate path makes resolve (first match wins) and checkout (last
    /// write wins) disagree about a version's content.
    pub fn validate_canonical(&self) -> Result<()> {
        for pair in self.entries.windows(2) {
            match pair[0].relative_path.cmp(&pair[1].relative_path) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    bail!("manifest lists {} more than once", pair[1].relative_path);
                }
                std::cmp::Ordering::Greater => {
                    bail!(
                        "manifest entries must be sorted by relative_path ({} listed after {})",
                        pair[1].relative_path,
                        pair[0].relative_path
                    );
                }
            }
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.size).sum()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub relative_path: String,
    pub sha256: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct AddOutcome {
    pub created: bool,
    pub version: VersionId,
    pub manifest_hash: String,
    pub file_count: u64,
    pub total_bytes: u64,
}

/// One registered write of a department's WIP stream (schema v8). Shares the
/// manifest/object machinery with publish versions but lives in a separate,
/// local-only, garbage-collected key space.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WipRecord {
    pub department_key: DepartmentKey,
    pub seq: u64,
    pub manifest_hash: String,
    pub created_at: String,
    pub source_path: String,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct WipOutcome {
    pub created: bool,
    pub seq: u64,
    pub manifest_hash: String,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct GcOutcome {
    pub dry_run: bool,
    pub retained_objects: u64,
    pub deleted_objects: u64,
    pub deleted_bytes: u64,
    pub pruned_wips: u64,
    pub pruned_manifests: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheGcOutcome {
    pub dry_run: bool,
    pub retained_views: u64,
    pub deleted_views: u64,
    pub retained_blobs: u64,
    pub deleted_blobs: u64,
    pub deleted_bytes: u64,
    pub deleted_staging_runs: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreateAssetOutcome {
    pub asset: AssetRecord,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct MaterializeOutcome {
    pub version: VersionId,
    pub path: PathBuf,
    pub unchanged: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishValidateReport {
    pub target: String,
    pub files_scanned: u64,
    pub references_checked: u64,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AssetInfo {
    pub asset: AssetRecord,
    pub current_versions: BTreeMap<String, VersionId>,
    pub versions: Vec<VersionRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: VersionRecord,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CurrentStatus {
    pub department_key: DepartmentKey,
    pub current: Option<VersionId>,
    pub latest: Option<VersionId>,
    pub explicit: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThumbnailRecord {
    pub department_key: DepartmentKey,
    pub version: VersionId,
    pub sha256: String,
    pub size: u64,
    pub mime_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub created_at: String,
    pub source_path: String,
}

#[derive(Clone, Debug)]
struct ResolvedAssetPath {
    department_key: DepartmentKey,
    version: VersionSelector,
    relative_path: String,
}

#[derive(Clone, Debug)]
struct ThumbnailImageInfo {
    mime_type: String,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    pub manifest_count: u64,
    pub version_count: u64,
    pub thumbnail_count: u64,
    pub objects_checked: u64,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug)]
struct ServeConfig {
    bind: SocketAddr,
    auth_token: String,
    profiles: BTreeMap<String, ServeProfile>,
    max_upload_bytes: usize,
    max_object_upload_bytes: usize,
    cors_allow_origins: Vec<HeaderValue>,
}

#[derive(Clone, Debug)]
struct ServeProfile {
    name: String,
    store: PathBuf,
    workspace: PathBuf,
}

#[derive(Clone, Debug, Default)]
struct FetchVersionStats {
    objects_downloaded: u64,
    objects_reused: u64,
    bytes_downloaded: u64,
}

#[derive(Clone, Debug, Default)]
struct SyncStats {
    assets_seen: u64,
    versions_synced: u64,
    objects_downloaded: u64,
    objects_reused: u64,
    bytes_downloaded: u64,
    materialized: u64,
}

#[derive(Clone, Debug, Default)]
struct PushVersionStats {
    objects_uploaded: u64,
    objects_reused: u64,
    bytes_uploaded: u64,
    thumbnails_pushed: u64,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Clone, Debug, Deserialize)]
struct AssetsQuery {
    profile: String,
    q: Option<String>,
    category: Option<String>,
    asset_code: Option<String>,
    department: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct AssetQuery {
    profile: String,
    category: String,
    asset_code: String,
}

#[derive(Clone, Debug, Deserialize)]
struct VersionsQuery {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
}

#[derive(Clone, Debug, Deserialize)]
struct VersionInfoQuery {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    version: Option<VersionId>,
    latest: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
struct ObjectQuery {
    profile: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ObjectStatusQuery {
    profile: String,
    sha256: String,
    size: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct CurrentStatusQuery {
    profile: String,
    category: Option<String>,
    asset_code: Option<String>,
    department: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ResolveQuery {
    profile: String,
    asset_path: String,
    mode: Option<String>,
    remote_base_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ThumbnailUrlQuery {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    version: Option<VersionId>,
    latest: Option<bool>,
    remote_base_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CurrentUpdateRequest {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    version: Option<VersionId>,
    reset: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VersionImportRequest {
    profile: String,
    version_info: VersionInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThumbnailImportRequest {
    profile: String,
    thumbnail: ThumbnailRecord,
}

#[derive(Clone, Debug, Deserialize)]
struct WorkspacePullRequest {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    version: Option<VersionId>,
    latest: Option<bool>,
    force: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
struct WipsQuery {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
}

#[derive(Clone, Debug, Serialize)]
struct WipsResponse {
    wips: Vec<WipRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WipImportRequest {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    manifest: Manifest,
    /// Provenance label stored on the record. Defaults to the logical work
    /// line (category/asset_code/department), same as local `wip add`.
    source_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct PromoteRequest {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    wip_seq: Option<u64>,
    no_validate: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
struct PromoteResponse {
    outcome: AddOutcome,
    validation: Option<PublishValidateReport>,
}

#[derive(Clone, Debug, Deserialize)]
struct GcRequest {
    profile: String,
    retention: Option<usize>,
    grace_hours: Option<u64>,
    dry_run: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
struct ProfileDto {
    name: String,
    store: String,
    workspace: String,
}

#[derive(Clone, Debug, Serialize)]
struct ProfilesResponse {
    profiles: Vec<ProfileDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AssetCardDto {
    category: String,
    asset_code: String,
    department: String,
    current: Option<VersionId>,
    latest: Option<VersionId>,
    explicit_current: bool,
    version_count: usize,
    latest_created_at: Option<String>,
    latest_file_count: Option<u64>,
    latest_total_bytes: Option<u64>,
    thumbnail_url: Option<String>,
    thumbnail_sha256: Option<String>,
    thumbnail_mime_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AssetsResponse {
    assets: Vec<AssetCardDto>,
}

#[derive(Clone, Debug, Serialize)]
struct AssetDetailResponse {
    info: AssetInfo,
    current_status: Vec<CurrentStatus>,
    thumbnails: Vec<ThumbnailRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct VersionsResponse {
    versions: Vec<VersionRecord>,
    current_status: CurrentStatus,
    thumbnails: Vec<ThumbnailRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ObjectStatusResponse {
    sha256: String,
    exists: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ObjectUploadResponse {
    sha256: String,
    size: u64,
    reused: bool,
}

#[derive(Clone, Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

/// Marker for "the requested entity does not exist" failures. The crate is
/// anyhow-everywhere, so the web layer cannot type-match domain errors;
/// carrying the original message inside this marker lets it walk the chain
/// and map these sites to HTTP 404 without rewording anything.
#[derive(Debug)]
struct NotFound(String);

impl fmt::Display for NotFound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

fn not_found_error(message: String) -> anyhow::Error {
    anyhow::Error::new(NotFound(message))
}

pub struct Store {
    root: PathBuf,
    db: DB,
}

impl Store {
    pub fn init(path: &Path) -> Result<Self> {
        // RocksDB creates the db directory eagerly on open, and a
        // just-created DB is indistinguishable from a foreign or corrupt one
        // afterwards (both have no keys). Decide fresh-vs-existing before
        // opening so init never re-stamps the schema marker on a store it
        // did not create — that would bypass the schema gate without
        // migrating anything.
        let db_preexists = db_path(path)
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);

        fs::create_dir_all(path)
            .with_context(|| format!("failed to create store root {}", path.display()))?;
        fs::create_dir_all(objects_root(path)).with_context(|| {
            format!(
                "failed to create objects directory under {}",
                path.display()
            )
        })?;
        fs::create_dir_all(db_path(path))
            .with_context(|| format!("failed to create db directory under {}", path.display()))?;

        let mut options = Options::default();
        options.create_if_missing(true);
        let db = DB::open(&options, db_path(path))
            .with_context(|| format!("failed to open RocksDB at {}", db_path(path).display()))?;
        if db_preexists {
            match db.get(key_meta("schema_version"))? {
                Some(schema) if schema.as_slice() == SCHEMA_VERSION.as_bytes() => {}
                Some(schema) => bail!(
                    "unsupported store schema version {}; expected {SCHEMA_VERSION} — init does not migrate existing stores; recreate the store at {}",
                    String::from_utf8_lossy(&schema),
                    path.display()
                ),
                None => bail!(
                    "existing database at {} has no schema_version marker; not an ADS store (or corrupt), refusing to initialize over it",
                    db_path(path).display()
                ),
            }
        } else {
            db.put(key_meta("schema_version"), SCHEMA_VERSION.as_bytes())?;
        }
        Ok(Self {
            root: path.to_path_buf(),
            db,
        })
    }

    pub fn open_or_init(path: &Path) -> Result<Self> {
        if db_path(path).exists() {
            Self::open(path)
        } else {
            Self::init(path)
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        if !db_path(path).exists() {
            bail!(
                "store is not initialized at {}; run `ads init {}` first",
                path.display(),
                path.display()
            );
        }
        let mut options = Options::default();
        options.create_if_missing(false);
        let db = DB::open(&options, db_path(path))
            .with_context(|| format!("failed to open RocksDB at {}", db_path(path).display()))?;
        Self::validate_schema(&db)?;
        Ok(Self {
            root: path.to_path_buf(),
            db,
        })
    }

    /// Opens the store read-only. Read-only opens do not take the RocksDB
    /// LOCK file, so read commands (resolve, list, info, checkout,
    /// materialize, ...) keep working while `ads serve` or another writer
    /// holds the store.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        if !db_path(path).exists() {
            bail!(
                "store is not initialized at {}; run `ads init {}` first",
                path.display(),
                path.display()
            );
        }
        let options = Options::default();
        let db = DB::open_for_read_only(&options, db_path(path), false)
            .with_context(|| format!("failed to open RocksDB at {}", db_path(path).display()))?;
        Self::validate_schema(&db)?;
        Ok(Self {
            root: path.to_path_buf(),
            db,
        })
    }

    fn validate_schema(db: &DB) -> Result<()> {
        let schema = db
            .get(key_meta("schema_version"))?
            .ok_or_else(|| anyhow!("store metadata is missing schema_version"))?;
        if schema.as_slice() != SCHEMA_VERSION.as_bytes() {
            bail!(
                "unsupported store schema version {}; expected {SCHEMA_VERSION}",
                String::from_utf8_lossy(&schema)
            );
        }
        Ok(())
    }

    pub fn set_remote_base_url(&self, remote_base_url: &str) -> Result<String> {
        let remote_base_url = normalize_remote_base_url(remote_base_url)?;
        self.db
            .put(key_meta("remote_base_url"), remote_base_url.as_bytes())?;
        Ok(remote_base_url)
    }

    pub fn remote_base_url(&self) -> Result<Option<String>> {
        self.db
            .get(key_meta("remote_base_url"))?
            .map(|value| {
                String::from_utf8(value)
                    .map_err(anyhow::Error::from)
                    .context("remote base URL is not UTF-8")
            })
            .transpose()
    }

    pub fn create_asset(
        &self,
        workspace: &Path,
        asset_key: &AssetKey,
    ) -> Result<CreateAssetOutcome> {
        if self.asset_record(asset_key)?.is_some() {
            bail!(
                "asset already exists: {}/{}",
                asset_key.category,
                asset_key.asset_code
            );
        }

        let path = asset_folder(workspace, asset_key);
        ensure_checkout_dest_outside_store(&self.root, &path)?;
        if path.exists() {
            if !path.is_dir() {
                bail!(
                    "asset path exists and is not a directory: {}",
                    path.display()
                );
            }
            if !is_empty_dir(&path)? {
                bail!("asset path exists and is not empty: {}", path.display());
            }
        }
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create asset folder {}", path.display()))?;

        let asset = AssetRecord {
            asset_key: asset_key.clone(),
            created_at: Utc::now().to_rfc3339(),
            latest_versions: BTreeMap::new(),
        };
        self.db.put(
            key_asset(asset_key),
            serde_json::to_vec(&asset).context("failed to serialize asset record")?,
        )?;

        Ok(CreateAssetOutcome { asset, path })
    }

    pub fn add_version_folder(
        &self,
        workspace: &Path,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<AddOutcome> {
        let source = version_folder(workspace, department_key, version);
        self.add_version_from_source(
            &source,
            department_key,
            version,
            version_workspace_relative_path(department_key, version),
        )
    }

    /// Registers a version from an arbitrary source folder (schema v8: the
    /// standard workspace layout is no longer required).
    pub fn add_version_source(
        &self,
        source: &Path,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<AddOutcome> {
        self.add_version_from_source(
            source,
            department_key,
            version,
            source.display().to_string(),
        )
    }

    pub fn next_version(&self, department_key: &DepartmentKey) -> Result<VersionId> {
        Ok(self
            .latest_version(department_key)?
            .map_or(VersionId(1), VersionId::next))
    }

    pub fn import_version_info(&self, info: &VersionInfo) -> Result<()> {
        info.manifest.validate_canonical()?;
        let manifest_hash = info.manifest.canonical_hash()?;
        if manifest_hash != info.version.manifest_hash {
            bail!(
                "remote manifest hash mismatch for {}/{}/{} {}: record={}, computed={}",
                info.version.department_key.asset_key.category,
                info.version.department_key.asset_key.asset_code,
                info.version.department_key.department,
                info.version.version,
                info.version.manifest_hash,
                manifest_hash
            );
        }
        for entry in &info.manifest.entries {
            validate_sha256(&entry.sha256)?;
            validate_manifest_relative_path(&entry.relative_path)?;
        }

        match self.try_get_version(&info.version.department_key, info.version.version)? {
            Some(existing) if existing.manifest_hash != info.version.manifest_hash => {
                bail!(
                    "local version already exists with different manifest: {}/{}/{} {}",
                    info.version.department_key.asset_key.category,
                    info.version.department_key.asset_key.asset_code,
                    info.version.department_key.department,
                    info.version.version
                );
            }
            _ => {}
        }

        let previous_asset = self.asset_record(&info.version.department_key.asset_key)?;
        let mut latest_versions = previous_asset
            .as_ref()
            .map(|asset| asset.latest_versions.clone())
            .unwrap_or_default();
        let should_update_latest = latest_versions
            .get(&info.version.department_key.department)
            .is_none_or(|latest| *latest < info.version.version);
        if should_update_latest {
            latest_versions.insert(
                info.version.department_key.department.clone(),
                info.version.version,
            );
        }
        let asset = AssetRecord {
            asset_key: info.version.department_key.asset_key.clone(),
            created_at: previous_asset
                .map(|asset| asset.created_at)
                .unwrap_or_else(|| info.version.created_at.clone()),
            latest_versions,
        };

        let mut batch = WriteBatch::default();
        batch.put(
            key_manifest(&manifest_hash),
            serde_json::to_vec(&info.manifest).context("failed to serialize manifest")?,
        );
        batch.put(
            key_version(&info.version.department_key, info.version.version),
            serde_json::to_vec(&info.version).context("failed to serialize version record")?,
        );
        batch.put(
            key_asset(&info.version.department_key.asset_key),
            serde_json::to_vec(&asset).context("failed to serialize asset record")?,
        );
        if should_update_latest {
            batch.put(
                key_latest(&info.version.department_key),
                info.version.version.0.to_string().as_bytes(),
            );
        }
        batch.put(
            key_manifest_index(&info.version.department_key, &manifest_hash),
            info.version.version.0.to_string().as_bytes(),
        );
        self.db.write(batch)?;
        Ok(())
    }

    fn wip_head_seq(&self, department_key: &DepartmentKey) -> Result<Option<u64>> {
        self.db
            .get(key_wip_head(department_key))?
            .map(|value| {
                let value = std::str::from_utf8(&value).context("wip head is not UTF-8")?;
                value.parse::<u64>().context("invalid wip head sequence")
            })
            .transpose()
    }

    pub fn get_wip(&self, department_key: &DepartmentKey, seq: u64) -> Result<WipRecord> {
        let value = self.db.get(key_wip(department_key, seq))?.ok_or_else(|| {
            not_found_error(format!(
                "wip not found: {}/{}/{} seq {}",
                department_key.asset_key.category,
                department_key.asset_key.asset_code,
                department_key.department,
                seq
            ))
        })?;
        serde_json::from_slice(&value).context("failed to decode wip record")
    }

    pub fn wip_head(&self, department_key: &DepartmentKey) -> Result<Option<WipRecord>> {
        match self.wip_head_seq(department_key)? {
            Some(seq) => Ok(Some(self.get_wip(department_key, seq)?)),
            None => Ok(None),
        }
    }

    pub fn list_wips(&self, department_key: &DepartmentKey) -> Result<Vec<WipRecord>> {
        let prefix = format!(
            "wip/{}/{}/{}/",
            department_key.asset_key.category,
            department_key.asset_key.asset_code,
            department_key.department
        );
        let mut records = Vec::new();
        for item in self
            .db
            .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
        {
            let (key, value) = item?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            let record: WipRecord = serde_json::from_slice(&value)
                .with_context(|| format!("failed to decode {}", String::from_utf8_lossy(&key)))?;
            if record.department_key != *department_key {
                continue;
            }
            records.push(record);
        }
        records.sort_by_key(|record| record.seq);
        Ok(records)
    }

    /// Registers one write of the WIP stream from an arbitrary source folder.
    /// Re-registering unchanged content returns the existing head instead of
    /// growing the stream.
    pub fn add_wip_from_source(
        &self,
        source: &Path,
        department_key: &DepartmentKey,
    ) -> Result<WipOutcome> {
        let source = source
            .canonicalize()
            .with_context(|| format!("wip source folder does not exist: {}", source.display()))?;
        if !source.is_dir() {
            bail!("wip source path is not a folder: {}", source.display());
        }
        // The record carries the logical work line, not the filesystem source:
        // staging folders are deleted right after registration, so their paths
        // would be dead the moment they were written.
        let source_path = format!(
            "{}/{}/{}",
            department_key.asset_key.category,
            department_key.asset_key.asset_code,
            department_key.department
        );

        let manifest = self.build_manifest(&source)?;
        self.add_wip_from_manifest(department_key, &manifest, source_path)
    }

    /// Registers one write of the WIP stream from an already-built manifest
    /// whose objects are in the store. Shared by `wip add` (which stages the
    /// objects while building the manifest) and `/api/wip` (whose objects
    /// were uploaded beforehand).
    pub fn add_wip_from_manifest(
        &self,
        department_key: &DepartmentKey,
        manifest: &Manifest,
        source_path: String,
    ) -> Result<WipOutcome> {
        manifest.validate_canonical()?;
        let manifest_hash = manifest.canonical_hash()?;

        if let Some(head) = self.wip_head(department_key)?
            && head.manifest_hash == manifest_hash
        {
            return Ok(WipOutcome {
                created: false,
                seq: head.seq,
                manifest_hash,
                file_count: head.file_count,
                total_bytes: head.total_bytes,
            });
        }

        let seq = self.wip_head_seq(department_key)?.unwrap_or(0) + 1;
        let record = WipRecord {
            department_key: department_key.clone(),
            seq,
            manifest_hash: manifest_hash.clone(),
            created_at: Utc::now().to_rfc3339(),
            source_path,
            file_count: manifest.entries.len() as u64,
            total_bytes: manifest.total_bytes(),
        };
        let mut batch = WriteBatch::default();
        batch.put(
            key_manifest(&manifest_hash),
            serde_json::to_vec(manifest).context("failed to serialize manifest")?,
        );
        batch.put(
            key_wip(department_key, seq),
            serde_json::to_vec(&record).context("failed to serialize wip record")?,
        );
        batch.put(key_wip_head(department_key), seq.to_string().as_bytes());
        self.db.write(batch)?;

        Ok(WipOutcome {
            created: true,
            seq,
            manifest_hash,
            file_count: record.file_count,
            total_bytes: record.total_bytes,
        })
    }

    /// Promotes a WIP micro-version to the next publish version. Metadata
    /// only: the manifest and objects are already in the store, so no file
    /// content is copied.
    pub fn promote_wip(
        &self,
        department_key: &DepartmentKey,
        wip_seq: Option<u64>,
    ) -> Result<AddOutcome> {
        let wip = match wip_seq {
            Some(seq) => self.get_wip(department_key, seq)?,
            None => self.wip_head(department_key)?.ok_or_else(|| {
                anyhow!(
                    "no wip versions to promote for {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                )
            })?,
        };

        if let Some(existing) =
            self.existing_manifest_version(department_key, &wip.manifest_hash)?
        {
            let record = self.get_version(department_key, existing)?;
            return Ok(AddOutcome {
                created: false,
                version: existing,
                manifest_hash: wip.manifest_hash,
                file_count: record.file_count,
                total_bytes: record.total_bytes,
            });
        }

        let version = self
            .latest_version(department_key)?
            .map_or(VersionId(1), VersionId::next);
        let now = Utc::now().to_rfc3339();
        let record = VersionRecord {
            department_key: department_key.clone(),
            version,
            manifest_hash: wip.manifest_hash.clone(),
            created_at: now.clone(),
            source_path: wip.source_path.clone(),
            file_count: wip.file_count,
            total_bytes: wip.total_bytes,
            promoted_from: Some(wip.seq),
        };
        let previous_asset = self.asset_record(&department_key.asset_key)?;
        let mut latest_versions = previous_asset
            .as_ref()
            .map(|asset| asset.latest_versions.clone())
            .unwrap_or_default();
        latest_versions.insert(department_key.department.clone(), version);
        let asset = AssetRecord {
            asset_key: department_key.asset_key.clone(),
            created_at: previous_asset.map(|asset| asset.created_at).unwrap_or(now),
            latest_versions,
        };

        let mut batch = WriteBatch::default();
        batch.put(
            key_version(department_key, version),
            serde_json::to_vec(&record).context("failed to serialize version record")?,
        );
        batch.put(
            key_asset(&department_key.asset_key),
            serde_json::to_vec(&asset).context("failed to serialize asset record")?,
        );
        batch.put(key_latest(department_key), version.0.to_string().as_bytes());
        batch.put(
            key_manifest_index(department_key, &wip.manifest_hash),
            version.0.to_string().as_bytes(),
        );
        self.db.write(batch)?;

        Ok(AddOutcome {
            created: true,
            version,
            manifest_hash: wip.manifest_hash,
            file_count: wip.file_count,
            total_bytes: wip.total_bytes,
        })
    }

    /// Mark-and-sweep garbage collection (schema v8 obligation: the WIP
    /// stream creates objects on every registered write).
    ///
    /// Roots are every publish version manifest, every thumbnail object, and
    /// the newest `wip_retention` micro-versions per department. WIP records
    /// past retention are pruned together with manifests referenced only by
    /// them. Unreferenced objects are deleted once older than `grace`
    /// (modification time), protecting writes that are racing the sweep.
    pub fn gc(
        &self,
        wip_retention: usize,
        grace: std::time::Duration,
        dry_run: bool,
    ) -> Result<GcOutcome> {
        let mut referenced_manifests: BTreeSet<String> = BTreeSet::new();
        let mut referenced_objects: BTreeSet<String> = BTreeSet::new();
        let mut wips_by_department: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        let mut wip_heads: BTreeMap<String, DepartmentKey> = BTreeMap::new();
        let mut all_manifest_hashes: Vec<String> = Vec::new();

        for item in self.db.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            if key.starts_with(b"version/") {
                let record: VersionRecord = serde_json::from_slice(&value).with_context(|| {
                    format!("failed to decode {}", String::from_utf8_lossy(&key))
                })?;
                referenced_manifests.insert(record.manifest_hash);
            } else if key.starts_with(b"thumbnail/") {
                let record: ThumbnailRecord =
                    serde_json::from_slice(&value).with_context(|| {
                        format!("failed to decode {}", String::from_utf8_lossy(&key))
                    })?;
                referenced_objects.insert(record.sha256);
            } else if key.starts_with(b"wip/") {
                let record: WipRecord = serde_json::from_slice(&value).with_context(|| {
                    format!("failed to decode {}", String::from_utf8_lossy(&key))
                })?;
                let department = format!(
                    "{}/{}/{}",
                    record.department_key.asset_key.category,
                    record.department_key.asset_key.asset_code,
                    record.department_key.department
                );
                wip_heads.insert(department.clone(), record.department_key.clone());
                wips_by_department
                    .entry(department)
                    .or_default()
                    .push((record.seq, record.manifest_hash));
            } else if key.starts_with(b"manifest/") {
                let hash = String::from_utf8_lossy(&key["manifest/".len()..]).to_string();
                all_manifest_hashes.push(hash);
            }
        }

        let mut pruned_wips = 0u64;
        let mut prune_batch = WriteBatch::default();
        for (department, mut wips) in wips_by_department {
            wips.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
            let department_key = &wip_heads[&department];
            for (index, (seq, manifest_hash)) in wips.into_iter().enumerate() {
                if index < wip_retention {
                    referenced_manifests.insert(manifest_hash);
                } else {
                    pruned_wips += 1;
                    prune_batch.delete(key_wip(department_key, seq));
                }
            }
            if wip_retention == 0 {
                prune_batch.delete(key_wip_head(department_key));
            }
        }

        let mut pruned_manifests = 0u64;
        for hash in all_manifest_hashes {
            if !referenced_manifests.contains(&hash) {
                pruned_manifests += 1;
                prune_batch.delete(key_manifest(&hash));
            }
        }
        if !dry_run {
            self.db.write(prune_batch)?;
        }

        for hash in &referenced_manifests {
            let manifest = self.get_manifest(hash)?;
            for entry in manifest.entries {
                referenced_objects.insert(entry.sha256);
            }
        }

        let mut retained_objects = 0u64;
        let mut deleted_objects = 0u64;
        let mut deleted_bytes = 0u64;
        let now = std::time::SystemTime::now();
        let objects_root = self.root.join(OBJECTS_DIR).join(SHA256_DIR);
        if objects_root.exists() {
            for entry in WalkDir::new(&objects_root).follow_links(false) {
                let entry =
                    entry.with_context(|| format!("failed to walk {}", objects_root.display()))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let sha = entry.file_name().to_string_lossy().to_string();
                if referenced_objects.contains(&sha) {
                    retained_objects += 1;
                    continue;
                }
                let metadata = entry
                    .metadata()
                    .with_context(|| format!("failed to stat {}", entry.path().display()))?;
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok());
                if age.is_none_or(|age| age < grace) {
                    retained_objects += 1;
                    continue;
                }
                deleted_objects += 1;
                deleted_bytes += metadata.len();
                if !dry_run {
                    fs::remove_file(entry.path()).with_context(|| {
                        format!("failed to delete object {}", entry.path().display())
                    })?;
                }
            }
        }

        Ok(GcOutcome {
            dry_run,
            retained_objects,
            deleted_objects,
            deleted_bytes,
            pruned_wips,
            pruned_manifests,
        })
    }

    /// Workspace cache garbage collection. The cache is rebuildable from the
    /// store, so this is purely a disk-space policy: keep the manifest views
    /// (and blobs) referenced by every department's latest, explicit current,
    /// and WIP head, delete the rest, and sweep staging runs and interrupted
    /// view materializations older than the grace window. Anything deleted
    /// re-materializes on the next resolve — including views for explicitly
    /// pinned old versions.
    ///
    /// Reads the store only, so it can run while `ads serve` holds it.
    pub fn cache_gc(
        &self,
        workspace: &Path,
        staging_grace: std::time::Duration,
        dry_run: bool,
    ) -> Result<CacheGcOutcome> {
        let mut department_keys: BTreeMap<String, DepartmentKey> = BTreeMap::new();
        let mut latest_by_department: BTreeMap<String, (VersionId, String)> = BTreeMap::new();
        for item in self.db.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            if key.starts_with(b"version/") {
                let record: VersionRecord = serde_json::from_slice(&value).with_context(|| {
                    format!("failed to decode {}", String::from_utf8_lossy(&key))
                })?;
                let department = format!(
                    "{}/{}/{}",
                    record.department_key.asset_key.category,
                    record.department_key.asset_key.asset_code,
                    record.department_key.department
                );
                department_keys
                    .entry(department.clone())
                    .or_insert_with(|| record.department_key.clone());
                let candidate = (record.version, record.manifest_hash);
                latest_by_department
                    .entry(department)
                    .and_modify(|current| {
                        if current.0 < candidate.0 {
                            *current = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            } else if key.starts_with(b"wip/") {
                let record: WipRecord = serde_json::from_slice(&value).with_context(|| {
                    format!("failed to decode {}", String::from_utf8_lossy(&key))
                })?;
                let department = format!(
                    "{}/{}/{}",
                    record.department_key.asset_key.category,
                    record.department_key.asset_key.asset_code,
                    record.department_key.department
                );
                department_keys
                    .entry(department)
                    .or_insert(record.department_key);
            }
        }

        let mut kept_manifests: BTreeSet<String> = BTreeSet::new();
        for (department, department_key) in &department_keys {
            if let Some((_, manifest_hash)) = latest_by_department.get(department) {
                kept_manifests.insert(manifest_hash.clone());
            }
            if let Some(version) = self.explicit_current_version(department_key)? {
                let record = self.get_version(department_key, version)?;
                kept_manifests.insert(record.manifest_hash);
            }
            if let Some(wip) = self.wip_head(department_key)? {
                kept_manifests.insert(wip.manifest_hash);
            }
        }

        let mut kept_blob_names: BTreeSet<String> = BTreeSet::new();
        for manifest_hash in &kept_manifests {
            let manifest = self.get_manifest(manifest_hash)?;
            for entry in &manifest.entries {
                kept_blob_names.insert(cache_blob_file_name(entry));
            }
        }

        let mut outcome = CacheGcOutcome {
            dry_run,
            ..CacheGcOutcome::default()
        };

        let manifests_root = workspace.join(CACHE_DIR).join(MANIFESTS_DIR);
        if manifests_root.exists() {
            let now = std::time::SystemTime::now();
            for entry in fs::read_dir(&manifests_root)
                .with_context(|| format!("failed to read {}", manifests_root.display()))?
            {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                if path.is_dir() {
                    if name.contains(".tmp.") {
                        // In-flight materializations build in sibling
                        // `<hash>.tmp.<pid>.<id>` folders that are renamed
                        // into place when complete. A live builder holds an
                        // exclusive lock on the sibling `.lock` file (see
                        // ensure_manifest_view); deleting under it would let
                        // the builder silently recreate the tree and publish
                        // a torn view with a valid marker, so a held lock
                        // wins over any age. The staging grace still covers
                        // pre-lock leftovers.
                        if view_build_lock_is_held(&manifests_root.join(format!("{name}.lock"))) {
                            continue;
                        }
                        let age = entry
                            .metadata()
                            .ok()
                            .and_then(|metadata| metadata.modified().ok())
                            .and_then(|modified| now.duration_since(modified).ok());
                        if age.is_none_or(|age| age < staging_grace) {
                            continue;
                        }
                    } else if kept_manifests.contains(&name) {
                        outcome.retained_views += 1;
                        continue;
                    }
                    outcome.deleted_views += 1;
                    for file in WalkDir::new(&path).follow_links(false) {
                        let file =
                            file.with_context(|| format!("failed to walk {}", path.display()))?;
                        if file.file_type().is_file() {
                            outcome.deleted_bytes += file.metadata().map(|m| m.len()).unwrap_or(0);
                        }
                    }
                    if !dry_run {
                        let _ = fs::remove_file(manifests_root.join(format!("{name}.complete")));
                        let _ = fs::remove_file(manifests_root.join(format!("{name}.lock")));
                        fs::remove_dir_all(&path)
                            .with_context(|| format!("failed to delete view {}", path.display()))?;
                    }
                } else if let Some(manifest_hash) = name.strip_suffix(".complete") {
                    // Orphan markers whose view folder is already gone.
                    if !kept_manifests.contains(manifest_hash)
                        && !manifests_root.join(manifest_hash).exists()
                        && !dry_run
                    {
                        let _ = fs::remove_file(&path);
                    }
                } else if let Some(temp_name) = name.strip_suffix(".lock") {
                    // Orphan build locks whose temp folder is already gone
                    // and whose builder no longer holds them.
                    if temp_name.contains(".tmp.")
                        && !manifests_root.join(temp_name).exists()
                        && !dry_run
                        && !view_build_lock_is_held(&path)
                    {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }

        let blobs_root = workspace.join(CACHE_DIR).join(SHA256_DIR);
        if blobs_root.exists() {
            for entry in WalkDir::new(&blobs_root).follow_links(false) {
                let entry =
                    entry.with_context(|| format!("failed to walk {}", blobs_root.display()))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if kept_blob_names.contains(&name) {
                    outcome.retained_blobs += 1;
                    continue;
                }
                outcome.deleted_blobs += 1;
                outcome.deleted_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                if !dry_run {
                    fs::remove_file(entry.path()).with_context(|| {
                        format!("failed to delete blob {}", entry.path().display())
                    })?;
                }
            }
        }

        let staging_root = workspace.join(STAGING_DIR);
        if staging_root.exists() {
            let now = std::time::SystemTime::now();
            for entry in fs::read_dir(&staging_root)
                .with_context(|| format!("failed to read {}", staging_root.display()))?
            {
                let entry = entry?;
                if !entry.path().is_dir() {
                    continue;
                }
                let age = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| now.duration_since(modified).ok());
                if age.is_none_or(|age| age < staging_grace) {
                    continue;
                }
                outcome.deleted_staging_runs += 1;
                if !dry_run {
                    fs::remove_dir_all(entry.path()).with_context(|| {
                        format!("failed to delete staging run {}", entry.path().display())
                    })?;
                }
            }
        }

        Ok(outcome)
    }

    fn add_version_from_source(
        &self,
        source: &Path,
        department_key: &DepartmentKey,
        version: VersionId,
        source_path: String,
    ) -> Result<AddOutcome> {
        let source = source
            .canonicalize()
            .with_context(|| format!("version folder does not exist: {}", source.display()))?;
        if !source.is_dir() {
            bail!("version path is not a folder: {}", source.display());
        }

        let manifest = self.build_manifest(&source)?;
        let manifest_hash = manifest.canonical_hash()?;

        if let Some(record) = self.try_get_version(department_key, version)? {
            if record.manifest_hash == manifest_hash {
                return Ok(AddOutcome {
                    created: false,
                    version,
                    manifest_hash,
                    file_count: record.file_count,
                    total_bytes: record.total_bytes,
                });
            }
            bail!(
                "version already exists with different content: {}/{}/{} {}",
                department_key.asset_key.category,
                department_key.asset_key.asset_code,
                department_key.department,
                version
            );
        }

        if let Some(existing) = self.existing_manifest_version(department_key, &manifest_hash)? {
            let record = self.get_version(department_key, existing)?;
            return Ok(AddOutcome {
                created: false,
                version: existing,
                manifest_hash,
                file_count: record.file_count,
                total_bytes: record.total_bytes,
            });
        }

        let latest = self.latest_version(department_key)?;
        let expected_version = latest.map_or(VersionId(1), VersionId::next);
        if version != expected_version {
            bail!(
                "version must be the next version for {}/{}/{}: expected {}, got {}",
                department_key.asset_key.category,
                department_key.asset_key.asset_code,
                department_key.department,
                expected_version,
                version
            );
        }

        let now = Utc::now().to_rfc3339();
        let record = VersionRecord {
            department_key: department_key.clone(),
            version,
            manifest_hash: manifest_hash.clone(),
            created_at: now.clone(),
            source_path,
            file_count: manifest.entries.len() as u64,
            total_bytes: manifest.total_bytes(),
            promoted_from: None,
        };
        let previous_asset = self.asset_record(&department_key.asset_key)?;
        let mut latest_versions = previous_asset
            .as_ref()
            .map(|asset| asset.latest_versions.clone())
            .unwrap_or_default();
        latest_versions.insert(department_key.department.clone(), version);
        let asset = AssetRecord {
            asset_key: department_key.asset_key.clone(),
            created_at: previous_asset.map(|asset| asset.created_at).unwrap_or(now),
            latest_versions,
        };

        let mut batch = WriteBatch::default();
        batch.put(
            key_manifest(&manifest_hash),
            serde_json::to_vec(&manifest).context("failed to serialize manifest")?,
        );
        batch.put(
            key_version(department_key, version),
            serde_json::to_vec(&record).context("failed to serialize version record")?,
        );
        batch.put(
            key_asset(&department_key.asset_key),
            serde_json::to_vec(&asset).context("failed to serialize asset record")?,
        );
        batch.put(key_latest(department_key), version.0.to_string().as_bytes());
        batch.put(
            key_manifest_index(department_key, &manifest_hash),
            version.to_string().as_bytes(),
        );
        self.db.write(batch)?;

        Ok(AddOutcome {
            created: true,
            version,
            manifest_hash,
            file_count: record.file_count,
            total_bytes: record.total_bytes,
        })
    }

    pub fn list_versions(
        &self,
        category: Option<&str>,
        asset_code: Option<&str>,
        department: Option<&str>,
    ) -> Result<Vec<VersionRecord>> {
        // v8 fixed-width keys keep a department's versions contiguous, so the
        // exact-department query (the hot /api/versions path) can prefix-seek
        // instead of scanning every record. Nested categories make raw key
        // prefixes ambiguous, so decoded records are still field-checked.
        if let (Some(category), Some(asset_code), Some(department)) =
            (category, asset_code, department)
        {
            let prefix = format!("version/{category}/{asset_code}/{department}/");
            let mut versions = Vec::new();
            for item in self
                .db
                .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
            {
                let (key, value) = item?;
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                let record: VersionRecord = serde_json::from_slice(&value).with_context(|| {
                    format!("failed to decode {}", String::from_utf8_lossy(&key))
                })?;
                if record.department_key.asset_key.category != category
                    || record.department_key.asset_key.asset_code != asset_code
                    || record.department_key.department != department
                {
                    continue;
                }
                versions.push(record);
            }
            versions.sort_by(|left, right| {
                left.department_key
                    .cmp(&right.department_key)
                    .then(left.version.cmp(&right.version))
            });
            return Ok(versions);
        }

        let mut versions = Vec::new();
        for item in self.db.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            if !key.starts_with(b"version/") {
                continue;
            }
            let record: VersionRecord = serde_json::from_slice(&value)
                .with_context(|| format!("failed to decode {}", String::from_utf8_lossy(&key)))?;
            if category.is_some_and(|category| record.department_key.asset_key.category != category)
            {
                continue;
            }
            if asset_code
                .is_some_and(|asset_code| record.department_key.asset_key.asset_code != asset_code)
            {
                continue;
            }
            if department.is_some_and(|department| record.department_key.department != department) {
                continue;
            }
            versions.push(record);
        }
        versions.sort_by(|left, right| {
            left.department_key
                .cmp(&right.department_key)
                .then(left.version.cmp(&right.version))
        });
        Ok(versions)
    }

    pub fn asset_info(&self, asset_key: &AssetKey) -> Result<AssetInfo> {
        let asset = self.asset_record(asset_key)?.ok_or_else(|| {
            not_found_error(format!(
                "asset not found: {}/{}",
                asset_key.category, asset_key.asset_code
            ))
        })?;
        let versions =
            self.list_versions(Some(&asset_key.category), Some(&asset_key.asset_code), None)?;
        let current_versions = self
            .current_status(Some(&asset_key.category), Some(&asset_key.asset_code), None)?
            .into_iter()
            .filter_map(|status| {
                status
                    .current
                    .map(|version| (status.department_key.department, version))
            })
            .collect();
        Ok(AssetInfo {
            asset,
            current_versions,
            versions,
        })
    }

    pub fn version_info(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<VersionInfo> {
        let version = self.get_version(department_key, version)?;
        let manifest = self.get_manifest(&version.manifest_hash)?;
        Ok(VersionInfo { version, manifest })
    }

    pub fn version_info_by_selector(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
    ) -> Result<VersionInfo> {
        let version = self
            .selected_version(department_key, selector)?
            .ok_or_else(|| {
                not_found_error(format!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                ))
            })?;
        self.version_info(department_key, version)
    }

    pub fn checkout(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
        dest: &Path,
        force: bool,
    ) -> Result<VersionRecord> {
        let version = self
            .selected_version(department_key, selector)?
            .ok_or_else(|| {
                anyhow!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                )
            })?;
        let record = self.get_version(department_key, version)?;
        let manifest = self.get_manifest(&record.manifest_hash)?;
        self.prepare_checkout_dest(dest, force)?;
        self.restore_manifest_to_dest(&manifest, dest)?;

        Ok(record)
    }

    /// Materializes a version into the department work folder
    /// `<workspace>/<category>/<asset_code>/<department>` — the same root the
    /// WIP staging processor redirects from, so a pull seeds the artist's
    /// working area for the wip-add/promote loop. Schema v8: no v### folder
    /// is created; explicit destinations use `checkout` instead.
    pub fn materialize(
        &self,
        workspace: &Path,
        department_key: &DepartmentKey,
        selector: VersionSelector,
        force: bool,
    ) -> Result<MaterializeOutcome> {
        let version = self
            .selected_version(department_key, selector)?
            .ok_or_else(|| {
                not_found_error(format!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                ))
            })?;
        let record = self.get_version(department_key, version)?;
        let manifest = self.get_manifest(&record.manifest_hash)?;
        let path = department_folder(workspace, department_key);

        if path.exists() && (path.is_file() || !is_empty_dir(&path)?) {
            if path.is_dir() && self.folder_matches_manifest(&path, &record.manifest_hash)? {
                return Ok(MaterializeOutcome {
                    version,
                    path,
                    unchanged: true,
                });
            }
            if !force {
                bail!(
                    "work folder exists and is not empty: {}; pass --force to replace it",
                    path.display()
                );
            }
        }

        self.prepare_checkout_dest(&path, force)?;
        self.restore_manifest_to_dest(&manifest, &path)?;

        Ok(MaterializeOutcome {
            version,
            path,
            unchanged: false,
        })
    }

    pub fn resolve_asset_path(
        &self,
        workspace: &Path,
        asset_path: &AssetPath,
        mode: ResolveMode,
        remote_base_url_override: Option<&str>,
    ) -> Result<ResolveOutcome> {
        let asset_path = self.resolve_asset_path_components(asset_path)?;
        if asset_path.version == VersionSelector::Wip {
            return self.resolve_wip_asset_path(workspace, &asset_path, mode);
        }
        let version = self
            .selected_version(&asset_path.department_key, asset_path.version)?
            .ok_or_else(|| {
                not_found_error(format!(
                    "department has no selected version: {}/{}/{}",
                    asset_path.department_key.asset_key.category,
                    asset_path.department_key.asset_key.asset_code,
                    asset_path.department_key.department
                ))
            })?;
        let record = self.get_version(&asset_path.department_key, version)?;
        let manifest = self.get_manifest(&record.manifest_hash)?;
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.relative_path == asset_path.relative_path)
            .ok_or_else(|| {
                not_found_error(format!(
                    "path not found in {}/{}/{} {}: {}",
                    asset_path.department_key.asset_key.category,
                    asset_path.department_key.asset_key.asset_code,
                    asset_path.department_key.department,
                    version,
                    asset_path.relative_path
                ))
            })?;
        let asset_file_kind = asset_file_kind(&asset_path.relative_path);

        // Local/auto resolve materializes from the store cache instead of the
        // workspace: leaf files resolve lazily to flat blob cache paths,
        // composing formats resolve into the immutable manifest view so
        // sibling-relative references keep working (schema v8: version
        // folders are no longer load targets). Auto falls back to the remote
        // object URL for both shapes when local objects are missing.
        match mode {
            ResolveMode::Local => {
                if asset_file_kind == AssetFileKind::Leaf {
                    let cache_path = self.ensure_cache_object(workspace, entry)?;
                    return Ok(ResolveOutcome {
                        location: cache_path.display().to_string(),
                        source: ResolveSource::Cache,
                        version: Some(version),
                        sha256: entry.sha256.clone(),
                    });
                }
                let view_root =
                    self.ensure_manifest_view(workspace, &record.manifest_hash, &manifest)?;
                let view_path = safe_join(&view_root, &asset_path.relative_path)?;
                Ok(ResolveOutcome {
                    location: view_path.display().to_string(),
                    source: ResolveSource::Cache,
                    version: Some(version),
                    sha256: entry.sha256.clone(),
                })
            }
            ResolveMode::Remote => Ok(ResolveOutcome {
                location: self.resolve_remote_url(entry, remote_base_url_override)?,
                source: ResolveSource::Remote,
                version: Some(version),
                sha256: entry.sha256.clone(),
            }),
            ResolveMode::Auto => {
                if asset_file_kind == AssetFileKind::Leaf {
                    return match self.ensure_cache_object(workspace, entry) {
                        Ok(cache_path) => Ok(ResolveOutcome {
                            location: cache_path.display().to_string(),
                            source: ResolveSource::Cache,
                            version: Some(version),
                            sha256: entry.sha256.clone(),
                        }),
                        Err(_) => Ok(ResolveOutcome {
                            location: self.resolve_remote_url(entry, remote_base_url_override)?,
                            source: ResolveSource::Remote,
                            version: Some(version),
                            sha256: entry.sha256.clone(),
                        }),
                    };
                }
                match self.ensure_manifest_view(workspace, &record.manifest_hash, &manifest) {
                    Ok(view_root) => {
                        let view_path = safe_join(&view_root, &asset_path.relative_path)?;
                        Ok(ResolveOutcome {
                            location: view_path.display().to_string(),
                            source: ResolveSource::Cache,
                            version: Some(version),
                            sha256: entry.sha256.clone(),
                        })
                    }
                    Err(_) => Ok(ResolveOutcome {
                        location: self.resolve_remote_url(entry, remote_base_url_override)?,
                        source: ResolveSource::Remote,
                        version: Some(version),
                        sha256: entry.sha256.clone(),
                    }),
                }
            }
        }
    }

    /// Resolves `?v=wip` to the department's WIP head. Local-only: remote
    /// mode is rejected and there is no remote fallback in auto mode.
    fn resolve_wip_asset_path(
        &self,
        workspace: &Path,
        asset_path: &ResolvedAssetPath,
        mode: ResolveMode,
    ) -> Result<ResolveOutcome> {
        if mode == ResolveMode::Remote {
            bail!("wip versions are local-only and cannot resolve in remote mode");
        }
        let wip = self.wip_head(&asset_path.department_key)?.ok_or_else(|| {
            not_found_error(format!(
                "department has no wip versions: {}/{}/{}",
                asset_path.department_key.asset_key.category,
                asset_path.department_key.asset_key.asset_code,
                asset_path.department_key.department
            ))
        })?;
        let manifest = self.get_manifest(&wip.manifest_hash)?;
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.relative_path == asset_path.relative_path)
            .ok_or_else(|| {
                not_found_error(format!(
                    "path not found in {}/{}/{} wip seq {}: {}",
                    asset_path.department_key.asset_key.category,
                    asset_path.department_key.asset_key.asset_code,
                    asset_path.department_key.department,
                    wip.seq,
                    asset_path.relative_path
                ))
            })?;
        let asset_file_kind = asset_file_kind(&asset_path.relative_path);
        if asset_file_kind == AssetFileKind::Leaf {
            let cache_path = self.ensure_cache_object(workspace, entry)?;
            return Ok(ResolveOutcome {
                location: cache_path.display().to_string(),
                source: ResolveSource::Cache,
                version: None,
                sha256: entry.sha256.clone(),
            });
        }
        let view_root = self.ensure_manifest_view(workspace, &wip.manifest_hash, &manifest)?;
        let view_path = safe_join(&view_root, &asset_path.relative_path)?;
        Ok(ResolveOutcome {
            location: view_path.display().to_string(),
            source: ResolveSource::Cache,
            version: None,
            sha256: entry.sha256.clone(),
        })
    }

    pub fn current_status_for_department(
        &self,
        department_key: &DepartmentKey,
    ) -> Result<CurrentStatus> {
        let latest = self.latest_version(department_key)?;
        let explicit_current = self.explicit_current_version(department_key)?;
        if let Some(version) = explicit_current {
            self.get_version(department_key, version)?;
        }
        Ok(CurrentStatus {
            department_key: department_key.clone(),
            current: explicit_current.or(latest),
            latest,
            explicit: explicit_current.is_some(),
        })
    }

    pub fn set_current_version(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<CurrentStatus> {
        self.get_version(department_key, version)?;
        self.db.put(
            key_current(department_key),
            version.0.to_string().as_bytes(),
        )?;
        self.current_status_for_department(department_key)
    }

    pub fn reset_current_version(&self, department_key: &DepartmentKey) -> Result<CurrentStatus> {
        self.db.delete(key_current(department_key))?;
        self.current_status_for_department(department_key)
    }

    pub fn current_status(
        &self,
        category: Option<&str>,
        asset_code: Option<&str>,
        department: Option<&str>,
    ) -> Result<Vec<CurrentStatus>> {
        let mut department_keys = BTreeMap::new();
        for record in self.list_versions(category, asset_code, department)? {
            department_keys.insert(record.department_key, ());
        }

        let mut statuses = Vec::new();
        for department_key in department_keys.into_keys() {
            statuses.push(self.current_status_for_department(&department_key)?);
        }
        statuses.sort_by(|left, right| left.department_key.cmp(&right.department_key));
        Ok(statuses)
    }

    pub fn set_thumbnail(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
        image: &Path,
    ) -> Result<ThumbnailRecord> {
        self.get_version(department_key, version)?;
        let image = image
            .canonicalize()
            .with_context(|| format!("thumbnail image does not exist: {}", image.display()))?;
        if !image.is_file() {
            bail!("thumbnail image is not a file: {}", image.display());
        }
        let image_info = inspect_thumbnail_image(&image)?;
        let (sha256, size) = hash_file(&image)?;
        self.ensure_object(&image, &sha256)?;

        let record = ThumbnailRecord {
            department_key: department_key.clone(),
            version,
            sha256,
            size,
            mime_type: image_info.mime_type,
            width: image_info.width,
            height: image_info.height,
            created_at: Utc::now().to_rfc3339(),
            source_path: image.display().to_string(),
        };
        self.db.put(
            key_thumbnail(department_key, version),
            serde_json::to_vec(&record).context("failed to serialize thumbnail record")?,
        )?;
        Ok(record)
    }

    pub fn import_thumbnail_info(&self, record: &ThumbnailRecord) -> Result<()> {
        self.get_version(&record.department_key, record.version)?;
        validate_sha256(&record.sha256)?;
        if !self.object_is_valid(&record.sha256, record.size)? {
            bail!(
                "thumbnail object missing or invalid for {}/{}/{} {}: {}",
                record.department_key.asset_key.category,
                record.department_key.asset_key.asset_code,
                record.department_key.department,
                record.version,
                record.sha256
            );
        }
        self.db.put(
            key_thumbnail(&record.department_key, record.version),
            serde_json::to_vec(record).context("failed to serialize thumbnail record")?,
        )?;
        Ok(())
    }

    pub fn thumbnail_info(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
    ) -> Result<ThumbnailRecord> {
        let version = self
            .selected_version(department_key, selector)?
            .ok_or_else(|| {
                not_found_error(format!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                ))
            })?;
        self.get_thumbnail(department_key, version)
    }

    pub fn copy_thumbnail(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
        dest: &Path,
        force: bool,
    ) -> Result<ThumbnailRecord> {
        let record = self.thumbnail_info(department_key, selector)?;
        let object_path = object_path(&self.root, &record.sha256);
        if !object_path.exists() {
            bail!(
                "thumbnail object is missing for {}/{}/{} {}: {}",
                record.department_key.asset_key.category,
                record.department_key.asset_key.asset_code,
                record.department_key.department,
                record.version,
                object_path.display()
            );
        }
        self.prepare_file_dest(dest, force)?;
        fs::copy(&object_path, dest).with_context(|| {
            format!(
                "failed to copy thumbnail object {} to {}",
                object_path.display(),
                dest.display()
            )
        })?;
        Ok(record)
    }

    pub fn list_thumbnails(
        &self,
        category: Option<&str>,
        asset_code: Option<&str>,
        department: Option<&str>,
    ) -> Result<Vec<ThumbnailRecord>> {
        // Same prefix-seek fast path as list_versions for the exact
        // department query.
        if let (Some(category), Some(asset_code), Some(department)) =
            (category, asset_code, department)
        {
            let prefix = format!("thumbnail/{category}/{asset_code}/{department}/");
            let mut records = Vec::new();
            for item in self
                .db
                .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
            {
                let (key, value) = item?;
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                let record: ThumbnailRecord =
                    serde_json::from_slice(&value).with_context(|| {
                        format!("failed to decode {}", String::from_utf8_lossy(&key))
                    })?;
                if record.department_key.asset_key.category != category
                    || record.department_key.asset_key.asset_code != asset_code
                    || record.department_key.department != department
                {
                    continue;
                }
                records.push(record);
            }
            records.sort_by(|left, right| {
                left.department_key
                    .cmp(&right.department_key)
                    .then(left.version.cmp(&right.version))
            });
            return Ok(records);
        }

        let mut records = Vec::new();
        for item in self.db.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            if !key.starts_with(b"thumbnail/") {
                continue;
            }
            let record: ThumbnailRecord = serde_json::from_slice(&value)
                .with_context(|| format!("failed to decode {}", String::from_utf8_lossy(&key)))?;
            if category.is_some_and(|category| record.department_key.asset_key.category != category)
            {
                continue;
            }
            if asset_code
                .is_some_and(|asset_code| record.department_key.asset_key.asset_code != asset_code)
            {
                continue;
            }
            if department.is_some_and(|department| record.department_key.department != department) {
                continue;
            }
            records.push(record);
        }
        records.sort_by(|left, right| {
            left.department_key
                .cmp(&right.department_key)
                .then(left.version.cmp(&right.version))
        });
        Ok(records)
    }

    pub fn thumbnail_url(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
        remote_base_url_override: Option<&str>,
    ) -> Result<String> {
        let record = self.thumbnail_info(department_key, selector)?;
        self.resolve_remote_sha256_url(&record.sha256, remote_base_url_override)
    }

    fn ensure_cache_object(&self, workspace: &Path, entry: &ManifestEntry) -> Result<PathBuf> {
        let object_path = object_path(&self.root, &entry.sha256);
        if !object_path.exists() {
            bail!(
                "object is missing for {}: {}",
                entry.relative_path,
                object_path.display()
            );
        }

        let cache_path = cache_object_path(workspace, entry);
        if cache_path.exists() {
            let metadata = fs::metadata(&cache_path)
                .with_context(|| format!("failed to stat {}", cache_path.display()))?;
            if metadata.is_file() && metadata.len() == entry.size {
                return Ok(cache_path);
            }
            if metadata.is_dir() {
                bail!(
                    "cache path exists and is a directory: {}",
                    cache_path.display()
                );
            }
            fs::remove_file(&cache_path).with_context(|| {
                format!("failed to remove stale cache {}", cache_path.display())
            })?;
        }

        let parent = cache_path
            .parent()
            .ok_or_else(|| anyhow!("invalid cache path: {}", cache_path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
        let temp_path = temp_sibling_path(&cache_path);
        fs::copy(&object_path, &temp_path).with_context(|| {
            format!(
                "failed to copy object {} to cache {}",
                object_path.display(),
                temp_path.display()
            )
        })?;
        match fs::rename(&temp_path, &cache_path) {
            Ok(()) => Ok(cache_path),
            Err(_) if cache_path.exists() => {
                let _ = fs::remove_file(&temp_path);
                Ok(cache_path)
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to move temporary cache {} to {}",
                    temp_path.display(),
                    cache_path.display()
                )
            }),
        }
    }

    /// Materializes an immutable folder-shaped view of a manifest under
    /// `<workspace>/.ads-cache/manifests/<manifest_hash>/`. Each entry is a
    /// hardlink to its blob in the sha256 cache (copy fallback), so relative
    /// references between files keep working and unchanged files cost no disk.
    /// The view is built in a sibling `<hash>.tmp.<pid>.<id>` folder with its
    /// completeness marker inside, then renamed into place — `cache gc` runs
    /// concurrently with resolve, so a marker must never become visible ahead
    /// of the files it vouches for. Legacy views guarded by a sibling
    /// `<manifest_hash>.complete` marker are still trusted as-is.
    fn ensure_manifest_view(
        &self,
        workspace: &Path,
        manifest_hash: &str,
        manifest: &Manifest,
    ) -> Result<PathBuf> {
        let view_root = manifest_view_root(workspace, manifest_hash);
        if view_root.join(VIEW_COMPLETE_MARKER).exists()
            || manifest_view_marker(workspace, manifest_hash).exists()
        {
            return Ok(view_root);
        }
        // A view folder without either marker is an interrupted or gc-torn
        // materialization; it cannot be resumed safely, so rebuild it.
        if view_root.exists() {
            fs::remove_dir_all(&view_root).with_context(|| {
                format!("failed to remove incomplete view {}", view_root.display())
            })?;
        }
        let temp_root = temp_sibling_path(&view_root);
        // A concurrent `cache gc` must never sweep this build mid-flight:
        // create_dir_all below would silently repair the tree and a torn
        // view would be published with a valid marker. The lock is taken
        // before the folder exists (so gc can never observe an unlocked
        // in-flight build) and held until the rename settles; gc skips temp
        // folders whose lock it cannot acquire.
        let lock_path = view_build_lock_path(&temp_root);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create view directory {}", parent.display()))?;
        }
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to create view lock {}", lock_path.display()))?;
        lock_file
            .try_lock()
            .with_context(|| format!("failed to lock view lock {}", lock_path.display()))?;
        let build = || -> Result<PathBuf> {
            fs::create_dir_all(&temp_root).with_context(|| {
                format!("failed to create view directory {}", temp_root.display())
            })?;
            for entry in &manifest.entries {
                // The marker write below would truncate the hardlinked blob.
                if entry
                    .relative_path
                    .eq_ignore_ascii_case(VIEW_COMPLETE_MARKER)
                {
                    bail!(
                        "manifest entry collides with the view marker name: {}",
                        entry.relative_path
                    );
                }
                let link_path = safe_join(&temp_root, &entry.relative_path)?;
                let blob_path = self.ensure_cache_object(workspace, entry)?;
                let parent = link_path
                    .parent()
                    .ok_or_else(|| anyhow!("invalid view path: {}", link_path.display()))?;
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create view directory {}", parent.display())
                })?;
                if fs::hard_link(&blob_path, &link_path).is_err() {
                    fs::copy(&blob_path, &link_path).with_context(|| {
                        format!(
                            "failed to copy blob {} into manifest view {}",
                            blob_path.display(),
                            link_path.display()
                        )
                    })?;
                }
            }
            let marker = temp_root.join(VIEW_COMPLETE_MARKER);
            fs::write(&marker, b"")
                .with_context(|| format!("failed to write view marker {}", marker.display()))?;
            match fs::rename(&temp_root, &view_root) {
                Ok(()) => Ok(view_root.clone()),
                // Another materializer renamed its build in first; published
                // views are complete by construction, so use it.
                Err(_) if view_root.exists() => {
                    let _ = fs::remove_dir_all(&temp_root);
                    Ok(view_root.clone())
                }
                Err(error) => {
                    let _ = fs::remove_dir_all(&temp_root);
                    Err(error).with_context(|| {
                        format!(
                            "failed to move manifest view {} to {}",
                            temp_root.display(),
                            view_root.display()
                        )
                    })
                }
            }
        };
        let result = build();
        drop(lock_file);
        let _ = fs::remove_file(&lock_path);
        result
    }

    pub fn remove_thumbnail(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
    ) -> Result<ThumbnailRecord> {
        let record = self.thumbnail_info(department_key, selector)?;
        self.db
            .delete(key_thumbnail(department_key, record.version))?;
        Ok(record)
    }

    fn resolve_remote_url(
        &self,
        entry: &ManifestEntry,
        remote_base_url_override: Option<&str>,
    ) -> Result<String> {
        self.resolve_remote_sha256_url(&entry.sha256, remote_base_url_override)
    }

    fn resolve_remote_sha256_url(
        &self,
        sha256: &str,
        remote_base_url_override: Option<&str>,
    ) -> Result<String> {
        let remote_base_url = match remote_base_url_override {
            Some(remote_base_url) => normalize_remote_base_url(remote_base_url)?,
            None => self.remote_base_url()?.ok_or_else(|| {
                anyhow!(
                    "remote base URL is not configured; use `ads set-remote` or --remote-base-url"
                )
            })?,
        };
        Ok(remote_object_url(&remote_base_url, sha256))
    }

    fn restore_manifest_to_dest(&self, manifest: &Manifest, dest: &Path) -> Result<()> {
        for entry in &manifest.entries {
            let object_path = object_path(&self.root, &entry.sha256);
            if !object_path.exists() {
                bail!(
                    "object is missing for {}: {}",
                    entry.relative_path,
                    object_path.display()
                );
            }
            let target = safe_join(dest, &entry.relative_path)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(&object_path, &target).with_context(|| {
                format!(
                    "failed to copy object {} to {}",
                    object_path.display(),
                    target.display()
                )
            })?;
            apply_simple_mode(&target, entry.mode)?;
        }
        Ok(())
    }

    pub fn verify(&self) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();

        for item in self.db.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            if key.starts_with(b"manifest/") {
                report.manifest_count += 1;
                let manifest_hash = String::from_utf8_lossy(&key["manifest/".len()..]).to_string();
                let manifest: Manifest = match serde_json::from_slice(&value) {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        report.errors.push(format!(
                            "manifest {manifest_hash} could not be decoded: {error}"
                        ));
                        continue;
                    }
                };
                match manifest.canonical_hash() {
                    Ok(actual) if actual == manifest_hash => {}
                    Ok(actual) => report.errors.push(format!(
                        "manifest {manifest_hash} hash mismatch; computed {actual}"
                    )),
                    Err(error) => report.errors.push(format!(
                        "manifest {manifest_hash} could not be hashed: {error}"
                    )),
                }
                for entry in &manifest.entries {
                    report.objects_checked += 1;
                    let path = object_path(&self.root, &entry.sha256);
                    if !path.exists() {
                        report.errors.push(format!(
                            "object missing for {} in manifest {}: {}",
                            entry.relative_path,
                            manifest_hash,
                            path.display()
                        ));
                        continue;
                    }
                    match hash_file(&path) {
                        Ok((actual_hash, actual_size)) => {
                            if actual_hash != entry.sha256 {
                                report.errors.push(format!(
                                    "object hash mismatch for {}: expected {}, computed {}",
                                    path.display(),
                                    entry.sha256,
                                    actual_hash
                                ));
                            }
                            if actual_size != entry.size {
                                report.errors.push(format!(
                                    "object size mismatch for {}: expected {}, got {}",
                                    path.display(),
                                    entry.size,
                                    actual_size
                                ));
                            }
                        }
                        Err(error) => report.errors.push(format!(
                            "object {} could not be read: {error}",
                            path.display()
                        )),
                    }
                }
            } else if key.starts_with(b"version/") {
                report.version_count += 1;
                match serde_json::from_slice::<VersionRecord>(&value) {
                    Ok(record) => {
                        if self.db.get(key_manifest(&record.manifest_hash))?.is_none() {
                            report.errors.push(format!(
                                "version {}/{}/{} {} references missing manifest {}",
                                record.department_key.asset_key.category,
                                record.department_key.asset_key.asset_code,
                                record.department_key.department,
                                record.version,
                                record.manifest_hash
                            ));
                        }
                    }
                    Err(error) => report.errors.push(format!(
                        "version {} could not be decoded: {error}",
                        String::from_utf8_lossy(&key)
                    )),
                }
            } else if key.starts_with(b"thumbnail/") {
                report.thumbnail_count += 1;
                match serde_json::from_slice::<ThumbnailRecord>(&value) {
                    Ok(record) => {
                        if self
                            .try_get_version(&record.department_key, record.version)?
                            .is_none()
                        {
                            report.errors.push(format!(
                                "thumbnail {}/{}/{} {} references missing version",
                                record.department_key.asset_key.category,
                                record.department_key.asset_key.asset_code,
                                record.department_key.department,
                                record.version
                            ));
                        }
                        report.objects_checked += 1;
                        let path = object_path(&self.root, &record.sha256);
                        if !path.exists() {
                            report.errors.push(format!(
                                "thumbnail object missing for {}/{}/{} {}: {}",
                                record.department_key.asset_key.category,
                                record.department_key.asset_key.asset_code,
                                record.department_key.department,
                                record.version,
                                path.display()
                            ));
                            continue;
                        }
                        match hash_file(&path) {
                            Ok((actual_hash, actual_size)) => {
                                if actual_hash != record.sha256 {
                                    report.errors.push(format!(
                                        "thumbnail object hash mismatch for {}: expected {}, computed {}",
                                        path.display(),
                                        record.sha256,
                                        actual_hash
                                    ));
                                }
                                if actual_size != record.size {
                                    report.errors.push(format!(
                                        "thumbnail object size mismatch for {}: expected {}, got {}",
                                        path.display(),
                                        record.size,
                                        actual_size
                                    ));
                                }
                            }
                            Err(error) => report.errors.push(format!(
                                "thumbnail object {} could not be read: {error}",
                                path.display()
                            )),
                        }
                    }
                    Err(error) => report.errors.push(format!(
                        "thumbnail {} could not be decoded: {error}",
                        String::from_utf8_lossy(&key)
                    )),
                }
            }
        }

        Ok(report)
    }

    fn resolve_asset_path_components(&self, asset_path: &AssetPath) -> Result<ResolvedAssetPath> {
        let mut candidates = Vec::new();
        for department_index in 1..asset_path.parts.len() {
            let asset_code = &asset_path.parts[department_index - 1];
            let department = &asset_path.parts[department_index];
            let mut version = asset_path.version;
            let mut relative_start = department_index + 1;
            if !asset_path.version_explicit
                && relative_start < asset_path.parts.len()
                && let Some(legacy_version) =
                    VersionSelector::parse(&asset_path.parts[relative_start])
            {
                version = legacy_version;
                relative_start += 1;
            }
            let relative_path = if relative_start >= asset_path.parts.len() {
                format!("{asset_code}.usd")
            } else {
                asset_path.parts[relative_start..].join("/")
            };
            if validate_asset_code(asset_code).is_err()
                || validate_department(department).is_err()
                || validate_manifest_relative_path(&relative_path).is_err()
            {
                continue;
            }

            if department_index == 1 {
                for department_key in
                    self.department_keys_by_asset_code_department(asset_code, department)?
                {
                    if self.selector_exists(&department_key, version)? {
                        candidates.push(ResolvedAssetPath {
                            department_key,
                            version,
                            relative_path: relative_path.clone(),
                        });
                    }
                }
            } else {
                let category = asset_path.parts[..department_index - 1].join("/");
                let Ok(asset_key) = AssetKey::new(category, asset_code.to_string()) else {
                    continue;
                };
                let Ok(department_key) = DepartmentKey::new(asset_key, department.to_string())
                else {
                    continue;
                };
                if self.selector_exists(&department_key, version)? {
                    candidates.push(ResolvedAssetPath {
                        department_key,
                        version,
                        relative_path: relative_path.clone(),
                    });
                }
            }
        }

        candidates.sort_by(|left, right| {
            left.department_key
                .cmp(&right.department_key)
                .then(left.relative_path.cmp(&right.relative_path))
        });
        candidates.dedup_by(|left, right| {
            left.department_key == right.department_key && left.relative_path == right.relative_path
        });

        match candidates.len() {
            0 => bail!("asset path could not be resolved to a registered asset"),
            1 => Ok(candidates.remove(0)),
            _ => {
                let matches = candidates
                    .iter()
                    .map(|candidate| {
                        format!(
                            "{}/{}/{}",
                            candidate.department_key.asset_key.category,
                            candidate.department_key.asset_key.asset_code,
                            candidate.department_key.department
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("ambiguous asset path; include category. matches: {matches}");
            }
        }
    }

    fn build_manifest(&self, source: &Path) -> Result<Manifest> {
        self.scan_manifest(source, true)
    }

    fn folder_matches_manifest(&self, source: &Path, manifest_hash: &str) -> Result<bool> {
        let manifest = self.scan_manifest(source, false)?;
        Ok(manifest.canonical_hash()? == manifest_hash)
    }

    fn scan_manifest(&self, source: &Path, persist_objects: bool) -> Result<Manifest> {
        let source_abs = source.canonicalize()?;
        let store_abs = self
            .root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize store {}", self.root.display()))?;
        if source_abs == store_abs || source_abs.starts_with(&store_abs) {
            bail!("source folder must not be the store or inside the store");
        }

        let ignore_rules = IgnoreRules::load(&source_abs)?;
        let cache_enabled = hash_cache_enabled();
        let index = if cache_enabled {
            load_hash_index(&source_abs)
        } else {
            HashIndex::default()
        };
        let mut fresh_index: BTreeMap<String, HashIndexEntry> = BTreeMap::new();
        let mut entries = Vec::new();
        for entry in WalkDir::new(&source_abs)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                should_descend_entry(entry, &source_abs, &store_abs, &ignore_rules)
            })
        {
            let entry =
                entry.with_context(|| format!("failed to walk {}", source_abs.display()))?;
            if entry.path() == source_abs {
                continue;
            }

            let file_type = entry.file_type();
            if file_type.is_dir() {
                continue;
            }
            if file_type.is_symlink() {
                bail!("symlinks are not supported: {}", entry.path().display());
            }
            if !file_type.is_file() {
                continue;
            }

            let rel_path = entry
                .path()
                .strip_prefix(&source_abs)
                .with_context(|| format!("failed to relativize {}", entry.path().display()))?;
            if is_default_ignored(rel_path, false) || ignore_rules.is_ignored(rel_path, false) {
                continue;
            }

            let relative_path = normalize_relative_path(rel_path)?;
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to stat {}", entry.path().display()))?;
            let stat = file_stat_key(&metadata);
            let cached_sha256 = stat.and_then(|(mtime_secs, mtime_nanos, size)| {
                index
                    .entries
                    .get(&relative_path)
                    .filter(|cached| {
                        cached.mtime_secs == mtime_secs
                            && cached.mtime_nanos == mtime_nanos
                            && cached.size == size
                    })
                    .map(|cached| cached.sha256.clone())
            });
            // Trust the memo only when the blob is already in the store (or
            // when nothing is persisted): an object must never be written
            // under a hash that was not computed from its bytes.
            let (sha256, size) = match cached_sha256 {
                Some(sha256) if !persist_objects || object_path(&self.root, &sha256).exists() => {
                    (sha256, metadata.len())
                }
                _ => hash_file(entry.path())?,
            };
            if persist_objects {
                self.ensure_object(entry.path(), &sha256)?;
            }
            if let Some((mtime_secs, mtime_nanos, stat_size)) = stat {
                fresh_index.insert(
                    relative_path.clone(),
                    HashIndexEntry {
                        mtime_secs,
                        mtime_nanos,
                        size: stat_size,
                        sha256: sha256.clone(),
                    },
                );
            }
            entries.push(ManifestEntry {
                relative_path,
                sha256,
                size,
                mode: simple_mode(entry.path())?,
            });
        }
        if cache_enabled && fresh_index != index.entries {
            store_hash_index(
                &source_abs,
                &HashIndex {
                    version: HASH_INDEX_VERSION,
                    entries: fresh_index,
                },
            );
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(Manifest { entries })
    }

    fn ensure_object(&self, source: &Path, sha256: &str) -> Result<()> {
        let path = object_path(&self.root, sha256);
        if path.exists() {
            return Ok(());
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("invalid object path: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create object directory {}", parent.display()))?;
        let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::copy(source, &temp_path).with_context(|| {
            format!(
                "failed to copy {} to temporary object {}",
                source.display(),
                temp_path.display()
            )
        })?;
        match fs::rename(&temp_path, &path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                let _ = fs::remove_file(&temp_path);
                Ok(())
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to move temporary object {} to {}",
                    temp_path.display(),
                    path.display()
                )
            }),
        }
    }

    fn object_is_valid(&self, sha256: &str, expected_size: u64) -> Result<bool> {
        validate_sha256(sha256)?;
        let path = object_path(&self.root, sha256);
        if !path.exists() {
            return Ok(false);
        }
        let metadata =
            fs::metadata(&path).with_context(|| format!("failed to stat {}", path.display()))?;
        if !metadata.is_file() || metadata.len() != expected_size {
            return Ok(false);
        }
        let (computed, _) = hash_file(&path)?;
        Ok(computed == sha256)
    }

    /// Streams `reader` into the object store: temp file next to the final
    /// path, incremental hash, then rename. The object never becomes visible
    /// under its sha256 before the bytes have been proven to match it.
    /// Returns the number of bytes written.
    fn write_object_from_reader(&self, sha256: &str, reader: &mut dyn Read) -> Result<u64> {
        validate_sha256(sha256)?;
        let path = object_path(&self.root, sha256);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("invalid object path: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create object directory {}", parent.display()))?;
        let temp_path = temp_sibling_path(&path);
        match copy_hashed_to_temp(reader, &temp_path, sha256) {
            Ok(size) => {
                self.finalize_object_temp(&temp_path, sha256)?;
                Ok(size)
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                Err(error)
            }
        }
    }

    /// Publishes a fully written, hash-verified temp file at its final
    /// object path. Windows cannot rename over a file that is open, so a
    /// stale object at the destination is removed first; the temp file is
    /// cleaned up if the rename still fails.
    fn finalize_object_temp(&self, temp_path: &Path, sha256: &str) -> Result<()> {
        let path = object_path(&self.root, sha256);
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        match fs::rename(temp_path, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(temp_path);
                Err(error).with_context(|| {
                    format!(
                        "failed to move temporary object {} to {}",
                        temp_path.display(),
                        path.display()
                    )
                })
            }
        }
    }

    fn prepare_checkout_dest(&self, dest: &Path, force: bool) -> Result<()> {
        ensure_checkout_dest_outside_store(&self.root, dest)?;
        if dest.exists() {
            if force {
                if dest.is_dir() {
                    fs::remove_dir_all(dest)
                        .with_context(|| format!("failed to remove {}", dest.display()))?;
                } else {
                    fs::remove_file(dest)
                        .with_context(|| format!("failed to remove {}", dest.display()))?;
                }
            } else if dest.is_file() || !is_empty_dir(dest)? {
                bail!(
                    "checkout destination exists and is not empty: {}; pass --force to replace it",
                    dest.display()
                );
            }
        }
        fs::create_dir_all(dest)
            .with_context(|| format!("failed to create checkout destination {}", dest.display()))?;
        Ok(())
    }

    fn prepare_file_dest(&self, dest: &Path, force: bool) -> Result<()> {
        ensure_checkout_dest_outside_store(&self.root, dest)?;
        if dest.exists() {
            if force {
                if dest.is_dir() {
                    bail!("destination is a directory: {}", dest.display());
                }
                fs::remove_file(dest)
                    .with_context(|| format!("failed to remove {}", dest.display()))?;
            } else {
                bail!(
                    "destination exists: {}; pass --force to replace it",
                    dest.display()
                );
            }
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        Ok(())
    }

    fn latest_version(&self, department_key: &DepartmentKey) -> Result<Option<VersionId>> {
        if let Some(value) = self.db.get(key_latest(department_key))? {
            let value = std::str::from_utf8(&value).context("latest version is not UTF-8")?;
            return Ok(Some(VersionId::from_str(value)?));
        }

        Ok(self
            .asset_record(&department_key.asset_key)?
            .and_then(|asset| {
                asset
                    .latest_versions
                    .get(&department_key.department)
                    .copied()
            }))
    }

    fn explicit_current_version(
        &self,
        department_key: &DepartmentKey,
    ) -> Result<Option<VersionId>> {
        self.db
            .get(key_current(department_key))?
            .map(|value| {
                let value = std::str::from_utf8(&value).context("current version is not UTF-8")?;
                VersionId::from_str(value)
            })
            .transpose()
    }

    fn current_version(&self, department_key: &DepartmentKey) -> Result<Option<VersionId>> {
        if let Some(version) = self.explicit_current_version(department_key)? {
            self.get_version(department_key, version)?;
            return Ok(Some(version));
        }
        self.latest_version(department_key)
    }

    fn selected_version(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
    ) -> Result<Option<VersionId>> {
        match selector {
            VersionSelector::Current => self.current_version(department_key),
            VersionSelector::Latest => self.latest_version(department_key),
            VersionSelector::Wip => bail!(
                "the wip selector is only supported by resolve; publish-tier operations need a published version"
            ),
            VersionSelector::Version(version) => Ok(self
                .try_get_version(department_key, version)?
                .map(|_| version)),
        }
    }

    /// Whether the selector resolves to anything on this department. Unlike
    /// `selected_version` this also understands the WIP stream, which has no
    /// publish version number.
    fn selector_exists(
        &self,
        department_key: &DepartmentKey,
        selector: VersionSelector,
    ) -> Result<bool> {
        match selector {
            VersionSelector::Wip => Ok(self.wip_head_seq(department_key)?.is_some()),
            other => Ok(self.selected_version(department_key, other)?.is_some()),
        }
    }

    fn department_keys_by_asset_code_department(
        &self,
        asset_code: &str,
        department: &str,
    ) -> Result<Vec<DepartmentKey>> {
        let mut department_keys = BTreeMap::new();
        for item in self.db.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            if !key.starts_with(b"version/") {
                continue;
            }
            let record: VersionRecord = serde_json::from_slice(&value)
                .with_context(|| format!("failed to decode {}", String::from_utf8_lossy(&key)))?;
            if record.department_key.asset_key.asset_code == asset_code
                && record.department_key.department == department
            {
                department_keys.insert(record.department_key, ());
            }
        }
        Ok(department_keys.into_keys().collect())
    }

    fn asset_record(&self, asset_key: &AssetKey) -> Result<Option<AssetRecord>> {
        self.db
            .get(key_asset(asset_key))?
            .map(|value| serde_json::from_slice(&value).context("failed to decode asset record"))
            .transpose()
    }

    fn existing_manifest_version(
        &self,
        department_key: &DepartmentKey,
        manifest_hash: &str,
    ) -> Result<Option<VersionId>> {
        self.db
            .get(key_manifest_index(department_key, manifest_hash))?
            .map(|value| {
                let value = std::str::from_utf8(&value).context("manifest index is not UTF-8")?;
                VersionId::from_str(value)
            })
            .transpose()
    }

    fn get_version(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<VersionRecord> {
        self.try_get_version(department_key, version)?
            .ok_or_else(|| {
                not_found_error(format!(
                    "version not found: {}/{}/{} {}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    version
                ))
            })
    }

    fn try_get_version(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<Option<VersionRecord>> {
        self.db
            .get(key_version(department_key, version))?
            .map(|value| serde_json::from_slice(&value).context("failed to decode version record"))
            .transpose()
    }

    fn get_thumbnail(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<ThumbnailRecord> {
        self.try_get_thumbnail(department_key, version)?
            .ok_or_else(|| {
                not_found_error(format!(
                    "thumbnail not found: {}/{}/{} {}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    version
                ))
            })
    }

    fn try_get_thumbnail(
        &self,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<Option<ThumbnailRecord>> {
        self.db
            .get(key_thumbnail(department_key, version))?
            .map(|value| {
                serde_json::from_slice(&value).context("failed to decode thumbnail record")
            })
            .transpose()
    }

    fn get_manifest(&self, manifest_hash: &str) -> Result<Manifest> {
        let value = self
            .db
            .get(key_manifest(manifest_hash))?
            .ok_or_else(|| not_found_error(format!("manifest not found: {manifest_hash}")))?;
        serde_json::from_slice(&value).context("failed to decode manifest")
    }
}

fn mib_to_bytes(value: u64, name: &str) -> Result<usize> {
    let bytes = value
        .checked_mul(1024)
        .and_then(|value| value.checked_mul(1024))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| anyhow!("{name} is too large"))?;
    if bytes == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(bytes)
}

impl ServeConfig {
    #[allow(clippy::too_many_arguments)]
    fn from_args(
        bind: SocketAddr,
        auth_token: Option<String>,
        profiles: Vec<String>,
        store: Option<PathBuf>,
        workspace: Option<PathBuf>,
        max_upload_mb: u64,
        max_object_upload_mb: u64,
        cors_allow_origins: Vec<String>,
    ) -> Result<Self> {
        let auth_token = auth_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow!("--auth-token or ADS_WEB_TOKEN is required for `ads serve`"))?;
        let max_upload_bytes = mib_to_bytes(max_upload_mb, "--max-upload-mb")?;
        let max_object_upload_bytes = mib_to_bytes(max_object_upload_mb, "--max-object-upload-mb")?;
        let cors_allow_origins = cors_allow_origins
            .iter()
            .map(|origin| {
                HeaderValue::from_str(origin)
                    .map_err(|_| anyhow!("invalid --cors-allow-origin value: {origin}"))
            })
            .collect::<Result<Vec<_>>>()?;

        let profiles = if profiles.is_empty() {
            let store = store.ok_or_else(|| {
                anyhow!("--store and --workspace are required when --profile is not used")
            })?;
            let workspace = workspace.ok_or_else(|| {
                anyhow!("--store and --workspace are required when --profile is not used")
            })?;
            let profile = ServeProfile::new(
                "default".to_string(),
                absolute_path(store)?,
                absolute_path(workspace)?,
            )?;
            BTreeMap::from([(profile.name.clone(), profile)])
        } else {
            if store.is_some() || workspace.is_some() {
                bail!("--profile cannot be combined with --store or --workspace");
            }
            let mut map = BTreeMap::new();
            for profile in profiles {
                let profile = ServeProfile::parse(&profile)?;
                if map.insert(profile.name.clone(), profile).is_some() {
                    bail!("duplicate profile name");
                }
            }
            map
        };

        Ok(Self {
            bind,
            auth_token,
            profiles,
            max_upload_bytes,
            max_object_upload_bytes,
            cors_allow_origins,
        })
    }
}

impl ServeProfile {
    fn new(name: String, store: PathBuf, workspace: PathBuf) -> Result<Self> {
        validate_profile_name(&name)?;
        Store::open(&store).with_context(|| {
            format!("failed to open profile `{name}` store {}", store.display())
        })?;
        Ok(Self {
            name,
            store,
            workspace,
        })
    }

    fn parse(value: &str) -> Result<Self> {
        let (name, paths) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("profile must use name=store::workspace: {value}"))?;
        let (store, workspace) = paths
            .split_once("::")
            .ok_or_else(|| anyhow!("profile must use name=store::workspace: {value}"))?;
        Self::new(
            name.to_string(),
            absolute_path(PathBuf::from(store))?,
            absolute_path(PathBuf::from(workspace))?,
        )
    }
}

#[derive(Debug)]
struct IgnoreRules {
    rules: Vec<IgnoreRule>,
}

impl IgnoreRules {
    fn load(root: &Path) -> Result<Self> {
        let path = root.join(".adsignore");
        if !path.exists() {
            return Ok(Self { rules: Vec::new() });
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut rules = Vec::new();
        for (line_number, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            rules.push(
                IgnoreRule::new(line)
                    .with_context(|| format!("invalid .adsignore line {}", line_number + 1))?,
            );
        }
        Ok(Self { rules })
    }

    fn is_ignored(&self, rel_path: &Path, is_dir: bool) -> bool {
        let Ok(rel_path) = normalize_relative_path(rel_path) else {
            return false;
        };
        self.rules
            .iter()
            .any(|rule| rule.matches(&rel_path, is_dir))
    }
}

#[derive(Debug)]
struct IgnoreRule {
    dir_only: bool,
    matchers: Vec<GlobMatcher>,
}

impl IgnoreRule {
    fn new(pattern: &str) -> Result<Self> {
        let mut pattern = pattern.replace('\\', "/");
        let dir_only = pattern.ends_with('/');
        if dir_only {
            pattern.pop();
        }
        let pattern = pattern.trim_start_matches('/');
        if pattern.is_empty() {
            bail!("empty pattern");
        }

        let matcher_patterns = if pattern.contains('/') {
            vec![pattern.to_string()]
        } else {
            vec![pattern.to_string(), format!("**/{pattern}")]
        };
        let matchers = matcher_patterns
            .into_iter()
            .map(|pattern| Glob::new(&pattern).map(|glob| glob.compile_matcher()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { dir_only, matchers })
    }

    fn matches(&self, rel_path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        self.matchers
            .iter()
            .any(|matcher| matcher.is_match(rel_path))
    }
}

fn print_version_table(
    versions: &[VersionRecord],
    current_versions: &BTreeMap<DepartmentKey, VersionId>,
) {
    println!(
        "{:<16} {:<20} {:<12} {:<8} {:<7} {:>8} {:>12} {:<25} MANIFEST",
        "CATEGORY",
        "ASSET_CODE",
        "DEPARTMENT",
        "VERSION",
        "CURRENT",
        "FILES",
        "BYTES",
        "CREATED_AT"
    );
    for record in versions {
        let current = current_versions
            .get(&record.department_key)
            .is_some_and(|current| *current == record.version);
        println!(
            "{:<16} {:<20} {:<12} {:<8} {:<7} {:>8} {:>12} {:<25} {}",
            record.department_key.asset_key.category,
            record.department_key.asset_key.asset_code,
            record.department_key.department,
            record.version,
            if current { "*" } else { "" },
            record.file_count,
            record.total_bytes,
            record.created_at,
            record.manifest_hash
        );
    }
}

fn print_current_status_table(statuses: &[CurrentStatus]) {
    println!(
        "{:<16} {:<20} {:<12} {:<8} {:<8} MODE",
        "CATEGORY", "ASSET_CODE", "DEPARTMENT", "CURRENT", "LATEST"
    );
    for status in statuses {
        println!(
            "{:<16} {:<20} {:<12} {:<8} {:<8} {}",
            status.department_key.asset_key.category,
            status.department_key.asset_key.asset_code,
            status.department_key.department,
            format_optional_version(status.current),
            format_optional_version(status.latest),
            if status.explicit {
                "explicit"
            } else {
                "follows-latest"
            }
        );
    }
}

fn print_thumbnail_table(records: &[ThumbnailRecord]) {
    println!(
        "{:<16} {:<20} {:<12} {:<8} {:<10} {:>8} {:>9} {:<25} SHA256",
        "CATEGORY",
        "ASSET_CODE",
        "DEPARTMENT",
        "VERSION",
        "MIME",
        "SIZE",
        "DIMENSIONS",
        "CREATED_AT"
    );
    for record in records {
        let dimensions = match (record.width, record.height) {
            (Some(width), Some(height)) => format!("{width}x{height}"),
            _ => "-".to_string(),
        };
        println!(
            "{:<16} {:<20} {:<12} {:<8} {:<10} {:>8} {:>9} {:<25} {}",
            record.department_key.asset_key.category,
            record.department_key.asset_key.asset_code,
            record.department_key.department,
            record.version,
            record.mime_type,
            record.size,
            dimensions,
            record.created_at,
            record.sha256
        );
    }
}

fn current_versions_by_department(
    statuses: &[CurrentStatus],
) -> BTreeMap<DepartmentKey, VersionId> {
    statuses
        .iter()
        .filter_map(|status| {
            status
                .current
                .map(|version| (status.department_key.clone(), version))
        })
        .collect()
}

fn format_optional_version(version: Option<VersionId>) -> String {
    version.map_or_else(|| "-".to_string(), |version| version.to_string())
}

fn should_descend_entry(
    entry: &DirEntry,
    source_abs: &Path,
    store_abs: &Path,
    ignore_rules: &IgnoreRules,
) -> bool {
    if entry.path() == source_abs {
        return true;
    }
    if entry.path().starts_with(store_abs) {
        return false;
    }
    let Ok(rel_path) = entry.path().strip_prefix(source_abs) else {
        return true;
    };
    if is_default_ignored(rel_path, entry.file_type().is_dir()) {
        return false;
    }
    if ignore_rules.is_ignored(rel_path, entry.file_type().is_dir()) {
        return false;
    }
    true
}

fn should_descend_upload_entry(
    entry: &DirEntry,
    source_abs: &Path,
    ignore_rules: &IgnoreRules,
) -> bool {
    if entry.path() == source_abs {
        return true;
    }
    let Ok(rel_path) = entry.path().strip_prefix(source_abs) else {
        return true;
    };
    if is_default_ignored(rel_path, entry.file_type().is_dir()) {
        return false;
    }
    if ignore_rules.is_ignored(rel_path, entry.file_type().is_dir()) {
        return false;
    }
    true
}

fn is_default_ignored(rel_path: &Path, is_dir: bool) -> bool {
    if rel_path.components().any(|component| {
        matches!(
            component,
            Component::Normal(value) if value == ".git" || value == ".svn" || value == ".hg" || value == CACHE_DIR
        )
    }) {
        return true;
    }

    let Some(file_name) = rel_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if file_name == ".adsignore" {
        return true;
    }
    if is_dir {
        return false;
    }
    matches!(file_name, ".DS_Store" | "Thumbs.db" | "desktop.ini")
        || file_name.ends_with('~')
        || file_name.ends_with(".tmp")
}

fn validate_asset_code(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("asset_code must not be empty");
    }
    if value.contains('/') || value.contains('\\') {
        bail!("asset_code must not contain path separators: {value}");
    }
    if value == "." || value == ".." {
        bail!("asset_code must not be . or ..");
    }
    Ok(())
}

fn validate_category(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("category must not be empty");
    }
    if value.contains('\\') {
        bail!("category must use '/' separators, not '\\': {value}");
    }
    for component in value.split('/') {
        if component.is_empty() {
            bail!("category must not contain empty path components: {value}");
        }
        if component == "." || component == ".." {
            bail!("category must not contain . or .. components: {value}");
        }
    }
    Ok(())
}

fn validate_department(value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("department must not be empty");
    }
    if value.contains('/') || value.contains('\\') {
        bail!("department must not contain path separators: {value}");
    }
    if value == "." || value == ".." {
        bail!("department must not be . or ..");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.chars().all(|character| character.is_ascii_hexdigit()) {
        bail!("sha256 must be 64 hexadecimal characters");
    }
    Ok(())
}

fn workspace_root(workspace: Option<PathBuf>) -> Result<PathBuf> {
    let workspace = workspace.unwrap_or(std::env::current_dir()?);
    if workspace.is_absolute() {
        Ok(workspace)
    } else {
        Ok(std::env::current_dir()?.join(workspace))
    }
}

fn asset_folder(workspace: &Path, asset_key: &AssetKey) -> PathBuf {
    let mut path = workspace.to_path_buf();
    push_category_path(&mut path, &asset_key.category);
    path.join(&asset_key.asset_code)
}

fn department_folder(workspace: &Path, department_key: &DepartmentKey) -> PathBuf {
    asset_folder(workspace, &department_key.asset_key).join(&department_key.department)
}

fn version_folder(workspace: &Path, department_key: &DepartmentKey, version: VersionId) -> PathBuf {
    department_folder(workspace, department_key).join(version.to_string())
}

fn push_category_path(path: &mut PathBuf, category: &str) {
    for component in category.split('/') {
        path.push(component);
    }
}

fn version_workspace_relative_path(department_key: &DepartmentKey, version: VersionId) -> String {
    format!(
        "{}/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department,
        version
    )
}

fn normalize_remote_base_url(remote_base_url: &str) -> Result<String> {
    let remote_base_url = remote_base_url.trim().trim_end_matches('/').to_string();
    if remote_base_url.is_empty() {
        bail!("remote base URL must not be empty");
    }
    if !remote_base_url.starts_with("http://") && !remote_base_url.starts_with("https://") {
        bail!("remote base URL must start with http:// or https://");
    }
    Ok(remote_base_url)
}

fn remote_object_url(remote_base_url: &str, sha256: &str) -> String {
    let prefix = sha256.get(0..2).unwrap_or("00");
    format!("{remote_base_url}/{prefix}/{sha256}")
}

fn url_encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn asset_file_kind(relative_path: &str) -> AssetFileKind {
    if let Some(extension) = normalized_extension(relative_path)
        && VIEW_EXTENSIONS.contains(&extension.as_str())
    {
        return AssetFileKind::Composing;
    }
    AssetFileKind::Leaf
}

fn normalized_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn manifest_view_root(workspace: &Path, manifest_hash: &str) -> PathBuf {
    workspace
        .join(CACHE_DIR)
        .join(MANIFESTS_DIR)
        .join(manifest_hash)
}

fn manifest_view_marker(workspace: &Path, manifest_hash: &str) -> PathBuf {
    workspace
        .join(CACHE_DIR)
        .join(MANIFESTS_DIR)
        .join(format!("{manifest_hash}.complete"))
}

/// Sibling lock file guarding an in-flight manifest-view build. The builder
/// holds an exclusive OS lock on it for the whole build so a concurrent
/// `cache gc` (possibly in another process, possibly with `--staging-hours
/// 0`) can tell a live build from a crashed one: the lock dies with the
/// builder's process, whereas the temp folder's mtime does not move for
/// writes in nested subdirs and its age says nothing at grace zero.
fn view_build_lock_path(temp_root: &Path) -> PathBuf {
    let mut file_name = temp_root
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    file_name.push(".lock");
    temp_root.with_file_name(file_name)
}

/// True when a live builder holds the build lock. A missing lock file means
/// no builder (pre-lock leftovers or already-cleaned builds); any other
/// failure to rule a builder out counts as held so gc never sweeps a build
/// it cannot prove dead.
fn view_build_lock_is_held(lock_path: &Path) -> bool {
    match fs::File::open(lock_path) {
        Ok(file) => match file.try_lock() {
            // Acquired: the builder is gone. The lock releases again when
            // `file` drops at the end of this match.
            Ok(()) => false,
            Err(_) => true,
        },
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

fn cache_object_path(workspace: &Path, entry: &ManifestEntry) -> PathBuf {
    let prefix = entry.sha256.get(0..2).unwrap_or("00");
    workspace
        .join(CACHE_DIR)
        .join(SHA256_DIR)
        .join(prefix)
        .join(cache_blob_file_name(entry))
}

fn cache_blob_file_name(entry: &ManifestEntry) -> String {
    let mut file_name = entry.sha256.clone();
    if let Some(extension) = normalized_extension(&entry.relative_path) {
        file_name.push('.');
        file_name.push_str(&extension);
    }
    file_name
}

fn inspect_thumbnail_image(path: &Path) -> Result<ThumbnailImageInfo> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if let Some((width, height)) = png_dimensions(&bytes) {
        return Ok(ThumbnailImageInfo {
            mime_type: "image/png".to_string(),
            width: Some(width),
            height: Some(height),
        });
    }
    if let Some((width, height)) = jpeg_dimensions(&bytes) {
        return Ok(ThumbnailImageInfo {
            mime_type: "image/jpeg".to_string(),
            width: Some(width),
            height: Some(height),
        });
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let dimensions = webp_dimensions(&bytes);
        return Ok(ThumbnailImageInfo {
            mime_type: "image/webp".to_string(),
            width: dimensions.map(|(width, _)| width),
            height: dimensions.map(|(_, height)| height),
        });
    }

    bail!(
        "unsupported thumbnail image format: {}; expected PNG, JPEG, or WebP",
        path.display()
    );
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut index = 2;
    while index + 4 <= bytes.len() {
        while index < bytes.len() && bytes[index] != 0xff {
            index += 1;
        }
        while index < bytes.len() && bytes[index] == 0xff {
            index += 1;
        }
        if index >= bytes.len() {
            return None;
        }
        let marker = bytes[index];
        index += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if index + 2 > bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes(bytes[index..index + 2].try_into().ok()?) as usize;
        if length < 2 || index + length > bytes.len() {
            return None;
        }
        if is_jpeg_sof_marker(marker) && length >= 7 {
            let height = u16::from_be_bytes(bytes[index + 3..index + 5].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[index + 5..index + 7].try_into().ok()?) as u32;
            return Some((width, height));
        }
        index += length;
    }
    None
}

fn is_jpeg_sof_marker(marker: u8) -> bool {
    matches!(
        marker,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 30 && &bytes[12..16] == b"VP8X" {
        let width = read_u24_le(&bytes[24..27])? + 1;
        let height = read_u24_le(&bytes[27..30])? + 1;
        return Some((width, height));
    }
    if bytes.len() >= 30 && &bytes[12..16] == b"VP8 " && &bytes[23..26] == b"\x9d\x01\x2a" {
        let width = u16::from_le_bytes(bytes[26..28].try_into().ok()?) as u32 & 0x3fff;
        let height = u16::from_le_bytes(bytes[28..30].try_into().ok()?) as u32 & 0x3fff;
        return Some((width, height));
    }
    if bytes.len() >= 25 && &bytes[12..16] == b"VP8L" && bytes[20] == 0x2f {
        let b0 = bytes[21] as u32;
        let b1 = bytes[22] as u32;
        let b2 = bytes[23] as u32;
        let b3 = bytes[24] as u32;
        let width = 1 + (((b1 & 0x3f) << 8) | b0);
        let height = 1 + (((b3 & 0x0f) << 10) | (b2 << 2) | ((b1 & 0xc0) >> 6));
        return Some((width, height));
    }
    None
}

fn read_u24_le(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 3 {
        return None;
    }
    Some(bytes[0] as u32 | ((bytes[1] as u32) << 8) | ((bytes[2] as u32) << 16))
}

fn objects_root(store: &Path) -> PathBuf {
    store.join(OBJECTS_DIR).join(SHA256_DIR)
}

fn db_path(store: &Path) -> PathBuf {
    store.join(DB_DIR)
}

pub fn object_path(store: &Path, sha256: &str) -> PathBuf {
    let prefix = sha256.get(0..2).unwrap_or("00");
    objects_root(store).join(prefix).join(sha256)
}

/// Temp name for an in-flight write, appended to the full file name — a
/// `with_extension` suffix would replace the blob extension, giving
/// `<sha>.tx` and `<sha>.png` the same temp path. A pid-only suffix is not
/// unique enough either: `ads serve` can run several writes of the same
/// target concurrently in one process.
fn temp_sibling_path(path: &Path) -> PathBuf {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    file_name.push(format!(".tmp.{}.{id}", std::process::id()));
    path.with_file_name(file_name)
}

/// Copies `reader` into `temp_path` while hashing incrementally, and fails
/// when the bytes do not match the claimed sha256 — callers only ever
/// publish verified temp files.
fn copy_hashed_to_temp(reader: &mut dyn Read, temp_path: &Path, sha256: &str) -> Result<u64> {
    let mut file = File::create(temp_path)
        .with_context(|| format!("failed to write temporary object {}", temp_path.display()))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 1024 * 64];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .context("failed to read object stream")?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        file.write_all(&buffer[..bytes_read])
            .with_context(|| format!("failed to write temporary object {}", temp_path.display()))?;
        size += bytes_read as u64;
    }
    // Close before the caller renames: Windows cannot move an open file.
    drop(file);
    let computed = hex::encode(hasher.finalize());
    if computed != sha256 {
        bail!("downloaded object hash mismatch: expected {sha256}, computed {computed}");
    }
    Ok(size)
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 1024 * 64];
    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        size += bytes_read as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Stat-based hash memo for a scanned source folder, stored at
/// `<source>/.ads-cache/hash-index.json` (a location every scan ignores).
/// A file whose (mtime, size) is unchanged since the previous scan reuses
/// its recorded sha256 instead of being re-read — the same trade git's
/// stat cache makes. `ADS_HASH_CACHE=0` disables both reads and writes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HashIndex {
    version: u32,
    entries: BTreeMap<String, HashIndexEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct HashIndexEntry {
    mtime_secs: u64,
    mtime_nanos: u32,
    size: u64,
    sha256: String,
}

const HASH_INDEX_VERSION: u32 = 1;
const HASH_INDEX_FILE: &str = "hash-index.json";

fn hash_cache_enabled() -> bool {
    std::env::var("ADS_HASH_CACHE").map_or(true, |value| value != "0")
}

fn hash_index_path(source_abs: &Path) -> PathBuf {
    source_abs.join(CACHE_DIR).join(HASH_INDEX_FILE)
}

fn load_hash_index(source_abs: &Path) -> HashIndex {
    let path = hash_index_path(source_abs);
    let Ok(bytes) = fs::read(&path) else {
        return HashIndex::default();
    };
    match serde_json::from_slice::<HashIndex>(&bytes) {
        Ok(index) if index.version == HASH_INDEX_VERSION => index,
        _ => HashIndex::default(),
    }
}

/// Best effort: a failure to persist the memo never fails the scan.
fn store_hash_index(source_abs: &Path, index: &HashIndex) {
    let path = hash_index_path(source_abs);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(index) else {
        return;
    };
    let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    if fs::write(&temp_path, bytes).is_err() {
        return;
    }
    if fs::rename(&temp_path, &path).is_err() {
        let _ = fs::remove_file(&temp_path);
    }
}

fn file_stat_key(metadata: &fs::Metadata) -> Option<(u64, u32, u64)> {
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((
        since_epoch.as_secs(),
        since_epoch.subsec_nanos(),
        metadata.len(),
    ))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn normalize_relative_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| anyhow!("paths must be valid UTF-8: {}", path.display()))?;
                parts.push(value);
            }
            _ => bail!(
                "relative path contains unsupported component: {}",
                path.display()
            ),
        }
    }
    Ok(parts.join("/"))
}

fn safe_join(root: &Path, rel_path: &str) -> Result<PathBuf> {
    validate_manifest_relative_path(rel_path)?;
    let mut result = root.to_path_buf();
    for component in Path::new(rel_path).components() {
        match component {
            Component::Normal(value) => result.push(value),
            _ => bail!("manifest path is not safe: {rel_path}"),
        }
    }
    Ok(result)
}

fn validate_manifest_relative_path(rel_path: &str) -> Result<()> {
    if rel_path.is_empty() {
        bail!("relative path must not be empty");
    }
    for component in Path::new(rel_path).components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!("relative path is not safe: {rel_path}"),
        }
    }
    Ok(())
}

fn is_empty_dir(path: &Path) -> Result<bool> {
    if !path.is_dir() {
        return Ok(false);
    }
    Ok(fs::read_dir(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .next()
        .is_none())
}

fn ensure_checkout_dest_outside_store(store: &Path, dest: &Path) -> Result<()> {
    let store_abs = store
        .canonicalize()
        .with_context(|| format!("failed to canonicalize store {}", store.display()))?;
    let dest_abs = absolute_existing_or_parent(dest)?;
    if dest_abs.starts_with(&store_abs) || store_abs.starts_with(&dest_abs) {
        bail!("checkout destination must not be inside or contain the store");
    }
    Ok(())
}

fn absolute_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    let mut cursor = absolute.as_path();
    while !cursor.exists() {
        if let Some(file_name) = cursor.file_name() {
            missing.push(file_name.to_os_string());
        }
        cursor = cursor
            .parent()
            .ok_or_else(|| anyhow!("failed to resolve {}", path.display()))?;
    }

    let mut resolved = cursor
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", cursor.display()))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn simple_mode(path: &Path) -> Result<u32> {
    let permissions = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(permissions.mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        if permissions.readonly() {
            Ok(0o444)
        } else {
            Ok(0o666)
        }
    }
}

fn apply_simple_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn key_meta(name: &str) -> String {
    format!("meta/{name}")
}

fn key_asset(asset_key: &AssetKey) -> String {
    format!("asset/{}/{}", asset_key.category, asset_key.asset_code)
}

fn key_version(department_key: &DepartmentKey, version: VersionId) -> String {
    format!(
        "version/{}/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department,
        version.key_encode()
    )
}

fn key_wip(department_key: &DepartmentKey, seq: u64) -> String {
    format!(
        "wip/{}/{}/{}/{:020}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department,
        seq
    )
}

fn key_wip_head(department_key: &DepartmentKey) -> String {
    format!(
        "wip_head/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department
    )
}

fn key_latest(department_key: &DepartmentKey) -> String {
    format!(
        "latest/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department
    )
}

fn key_current(department_key: &DepartmentKey) -> String {
    format!(
        "current/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department
    )
}

fn key_thumbnail(department_key: &DepartmentKey, version: VersionId) -> String {
    format!(
        "thumbnail/{}/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department,
        version.key_encode()
    )
}

fn key_manifest(manifest_hash: &str) -> String {
    format!("manifest/{manifest_hash}")
}

fn key_manifest_index(department_key: &DepartmentKey, manifest_hash: &str) -> String {
    format!(
        "manifest_index/{}/{}/{}/{}",
        department_key.asset_key.category,
        department_key.asset_key.asset_code,
        department_key.department,
        manifest_hash
    )
}

#[cfg(test)]
mod tests;
