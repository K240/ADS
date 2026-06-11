use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
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
use clap::{Args, Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobMatcher};
use rocksdb::{DB, Direction, IteratorMode, Options, WriteBatch};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use walkdir::{DirEntry, WalkDir};

const SCHEMA_VERSION: &str = "8";
const DB_DIR: &str = "db";
const OBJECTS_DIR: &str = "objects";
const SHA256_DIR: &str = "sha256";
const CACHE_DIR: &str = ".ads-cache";
const MANIFESTS_DIR: &str = "manifests";
const STAGING_DIR: &str = ".ads-staging";
const USD_EXTENSIONS: &[&str] = &["usd", "usda", "usdc", "usdz"];
/// Formats that can carry relative references to sibling files and therefore
/// resolve through the eagerly materialized manifest view. Everything else is
/// a leaf (textures, volumes, caches, ...) and resolves lazily to its flat
/// blob cache path — one file copied per request instead of the whole
/// version.
const VIEW_EXTENSIONS: &[&str] = &["usd", "usda", "usdc", "usdz", "mtlx"];

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
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
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
        #[arg(long, default_value_t = 20)]
        retention: usize,
        /// Grace period in hours: unreferenced objects newer than this are kept.
        #[arg(long = "grace-hours", default_value_t = 24)]
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
            workspace,
            category,
            asset_code,
            department,
            version,
            source,
        } => {
            let asset_key = AssetKey::new(category, asset_code)?;
            let department_key = DepartmentKey::new(asset_key, department)?;
            let store = Store::open(&store)?;
            let outcome = match source {
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
                category,
                asset_code,
                department,
                version,
                image,
            } => {
                let asset_key = AssetKey::new(category, asset_code)?;
                let department_key = DepartmentKey::new(asset_key, department)?;
                let store = Store::open(&store)?;
                let record = store.set_thumbnail(&department_key, version, &image)?;
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
        } => {
            let config = ServeConfig::from_args(
                bind,
                auth_token,
                profiles,
                store,
                workspace,
                max_upload_mb,
                max_object_upload_mb,
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
                    if outcome.created { "promoted" } else { "reused" },
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
            report
                .errors
                .push(format!("{rel_path} is a symlink; publish does not allow symlinks"));
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
    let mut references = Vec::new();
    let mut current = String::new();
    let mut in_reference = false;

    for character in text.chars() {
        if character == '@' {
            if in_reference {
                references.push(current.clone());
                current.clear();
                in_reference = false;
            } else {
                in_reference = true;
            }
            continue;
        }
        if in_reference {
            current.push(character);
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
        let bytes = remote.fetch_object(profile, &entry.sha256)?;
        stats.bytes_downloaded += bytes.len() as u64;
        store.write_object_bytes(&entry.sha256, &bytes)?;
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
        let bytes = store.read_object_bytes(&entry.sha256, entry.size)?;
        let upload = remote.upload_object(profile, &entry.sha256, &bytes)?;
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
            let bytes = store.read_object_bytes(&thumbnail.sha256, thumbnail.size)?;
            let upload = remote.upload_object(profile, &thumbnail.sha256, &bytes)?;
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
}

#[derive(Clone, Debug)]
struct ServeProfile {
    name: String,
    store: PathBuf,
    workspace: PathBuf,
}

#[derive(Clone)]
struct WebState {
    auth_token: String,
    profiles: Arc<BTreeMap<String, WebProfile>>,
    max_upload_bytes: usize,
    max_object_upload_bytes: usize,
}

#[derive(Clone)]
struct WebProfile {
    name: String,
    store: PathBuf,
    workspace: PathBuf,
    store_handle: Arc<Store>,
    mutation_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
struct RemoteClient {
    server: String,
    auth_token: String,
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

pub struct Store {
    root: PathBuf,
    db: DB,
}

impl Store {
    pub fn init(path: &Path) -> Result<Self> {
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
        db.put(key_meta("schema_version"), SCHEMA_VERSION.as_bytes())?;
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
            anyhow!(
                "wip not found: {}/{}/{} seq {}",
                department_key.asset_key.category,
                department_key.asset_key.asset_code,
                department_key.department,
                seq
            )
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
            serde_json::to_vec(&manifest).context("failed to serialize manifest")?,
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

        if let Some(existing) = self.existing_manifest_version(department_key, &wip.manifest_hash)?
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
                let record: ThumbnailRecord = serde_json::from_slice(&value).with_context(
                    || format!("failed to decode {}", String::from_utf8_lossy(&key)),
                )?;
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
                let entry = entry
                    .with_context(|| format!("failed to walk {}", objects_root.display()))?;
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
    /// and WIP head, delete the rest, and sweep staging runs older than the
    /// grace window. Anything deleted re-materializes on the next resolve —
    /// including views for explicitly pinned old versions.
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
            for entry in fs::read_dir(&manifests_root)
                .with_context(|| format!("failed to read {}", manifests_root.display()))?
            {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                if path.is_dir() {
                    if kept_manifests.contains(&name) {
                        outcome.retained_views += 1;
                        continue;
                    }
                    outcome.deleted_views += 1;
                    for file in WalkDir::new(&path).follow_links(false) {
                        let file = file
                            .with_context(|| format!("failed to walk {}", path.display()))?;
                        if file.file_type().is_file() {
                            outcome.deleted_bytes += file.metadata().map(|m| m.len()).unwrap_or(0);
                        }
                    }
                    if !dry_run {
                        let _ = fs::remove_file(manifests_root.join(format!("{name}.complete")));
                        fs::remove_dir_all(&path).with_context(|| {
                            format!("failed to delete view {}", path.display())
                        })?;
                    }
                } else if let Some(manifest_hash) = name.strip_suffix(".complete") {
                    // Orphan markers whose view folder is already gone.
                    if !kept_manifests.contains(manifest_hash)
                        && !manifests_root.join(manifest_hash).exists()
                        && !dry_run
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
                let record: VersionRecord = serde_json::from_slice(&value)
                    .with_context(|| format!("failed to decode {}", String::from_utf8_lossy(&key)))?;
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
            anyhow!(
                "asset not found: {}/{}",
                asset_key.category,
                asset_key.asset_code
            )
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
                anyhow!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                )
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
                anyhow!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                )
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
                anyhow!(
                    "department has no selected version: {}/{}/{}",
                    asset_path.department_key.asset_key.category,
                    asset_path.department_key.asset_key.asset_code,
                    asset_path.department_key.department
                )
            })?;
        let record = self.get_version(&asset_path.department_key, version)?;
        let manifest = self.get_manifest(&record.manifest_hash)?;
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.relative_path == asset_path.relative_path)
            .ok_or_else(|| {
                anyhow!(
                    "path not found in {}/{}/{} {}: {}",
                    asset_path.department_key.asset_key.category,
                    asset_path.department_key.asset_key.asset_code,
                    asset_path.department_key.department,
                    version,
                    asset_path.relative_path
                )
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
                            location: self
                                .resolve_remote_url(entry, remote_base_url_override)?,
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
            anyhow!(
                "department has no wip versions: {}/{}/{}",
                asset_path.department_key.asset_key.category,
                asset_path.department_key.asset_key.asset_code,
                asset_path.department_key.department
            )
        })?;
        let manifest = self.get_manifest(&wip.manifest_hash)?;
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.relative_path == asset_path.relative_path)
            .ok_or_else(|| {
                anyhow!(
                    "path not found in {}/{}/{} wip seq {}: {}",
                    asset_path.department_key.asset_key.category,
                    asset_path.department_key.asset_key.asset_code,
                    asset_path.department_key.department,
                    wip.seq,
                    asset_path.relative_path
                )
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
        self.db
            .put(key_current(department_key), version.0.to_string().as_bytes())?;
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
                anyhow!(
                    "department has no selected version: {}/{}/{}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department
                )
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
                let record: ThumbnailRecord = serde_json::from_slice(&value)
                    .with_context(|| format!("failed to decode {}", String::from_utf8_lossy(&key)))?;
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
        let temp_path = cache_path.with_extension(format!("tmp.{}", std::process::id()));
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
    /// The view is keyed by manifest hash and therefore never overwritten; a
    /// sibling `<manifest_hash>.complete` marker short-circuits repeat calls.
    fn ensure_manifest_view(
        &self,
        workspace: &Path,
        manifest_hash: &str,
        manifest: &Manifest,
    ) -> Result<PathBuf> {
        let view_root = manifest_view_root(workspace, manifest_hash);
        let marker = manifest_view_marker(workspace, manifest_hash);
        if marker.exists() {
            return Ok(view_root);
        }
        for entry in &manifest.entries {
            let link_path = safe_join(&view_root, &entry.relative_path)?;
            if let Ok(metadata) = fs::metadata(&link_path) {
                if metadata.is_file() && metadata.len() == entry.size {
                    continue;
                }
                if metadata.is_dir() {
                    bail!(
                        "manifest view path exists and is a directory: {}",
                        link_path.display()
                    );
                }
                fs::remove_file(&link_path).with_context(|| {
                    format!(
                        "failed to remove incomplete view file {}",
                        link_path.display()
                    )
                })?;
            }
            let blob_path = self.ensure_cache_object(workspace, entry)?;
            let parent = link_path
                .parent()
                .ok_or_else(|| anyhow!("invalid view path: {}", link_path.display()))?;
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create view directory {}", parent.display())
            })?;
            if fs::hard_link(&blob_path, &link_path).is_err() {
                if link_path.exists() {
                    continue;
                }
                fs::copy(&blob_path, &link_path).with_context(|| {
                    format!(
                        "failed to copy blob {} into manifest view {}",
                        blob_path.display(),
                        link_path.display()
                    )
                })?;
            }
        }
        let marker_parent = marker
            .parent()
            .ok_or_else(|| anyhow!("invalid view marker path: {}", marker.display()))?;
        fs::create_dir_all(marker_parent).with_context(|| {
            format!(
                "failed to create cache directory {}",
                marker_parent.display()
            )
        })?;
        fs::write(&marker, b"")
            .with_context(|| format!("failed to write view marker {}", marker.display()))?;
        Ok(view_root)
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
            let (sha256, size) = hash_file(entry.path())?;
            if persist_objects {
                self.ensure_object(entry.path(), &sha256)?;
            }
            entries.push(ManifestEntry {
                relative_path,
                sha256,
                size,
                mode: simple_mode(entry.path())?,
            });
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

    fn read_object_bytes(&self, sha256: &str, expected_size: u64) -> Result<Vec<u8>> {
        validate_sha256(sha256)?;
        let path = object_path(&self.root, sha256);
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        if bytes.len() as u64 != expected_size {
            bail!(
                "local object size mismatch: {} expected={} actual={}",
                sha256,
                expected_size,
                bytes.len()
            );
        }
        let computed = sha256_bytes(&bytes);
        if computed != sha256 {
            bail!("local object hash mismatch: expected {sha256}, computed {computed}");
        }
        Ok(bytes)
    }

    fn write_object_bytes(&self, sha256: &str, bytes: &[u8]) -> Result<()> {
        validate_sha256(sha256)?;
        let computed = sha256_bytes(bytes);
        if computed != sha256 {
            bail!("downloaded object hash mismatch: expected {sha256}, computed {computed}");
        }
        let path = object_path(&self.root, sha256);
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("invalid object path: {}", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create object directory {}", parent.display()))?;
        let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&temp_path, bytes)
            .with_context(|| format!("failed to write temporary object {}", temp_path.display()))?;
        match fs::rename(&temp_path, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
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
                anyhow!(
                    "version not found: {}/{}/{} {}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    version
                )
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
                anyhow!(
                    "thumbnail not found: {}/{}/{} {}",
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    version
                )
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
            .ok_or_else(|| anyhow!("manifest not found: {manifest_hash}"))?;
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
    fn from_args(
        bind: SocketAddr,
        auth_token: Option<String>,
        profiles: Vec<String>,
        store: Option<PathBuf>,
        workspace: Option<PathBuf>,
        max_upload_mb: u64,
        max_object_upload_mb: u64,
    ) -> Result<Self> {
        let auth_token = auth_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow!("--auth-token or ADS_WEB_TOKEN is required for `ads serve`"))?;
        let max_upload_bytes = mib_to_bytes(max_upload_mb, "--max-upload-mb")?;
        let max_object_upload_bytes = mib_to_bytes(max_object_upload_mb, "--max-object-upload-mb")?;

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

impl RemoteClient {
    fn new(server: &str, auth_token: &str) -> Result<Self> {
        let server = server.trim().trim_end_matches('/').to_string();
        if server.is_empty() {
            bail!("--server must not be empty");
        }
        if !server.starts_with("http://") && !server.starts_with("https://") {
            bail!("--server must start with http:// or https://");
        }
        let auth_token = auth_token.trim().to_string();
        if auth_token.is_empty() {
            bail!("--auth-token or ADS_WEB_TOKEN is required");
        }
        Ok(Self { server, auth_token })
    }

    fn fetch_version_info(
        &self,
        profile: &str,
        category: &str,
        asset_code: &str,
        department: &str,
        selector: VersionSelector,
    ) -> Result<VersionInfo> {
        let mut query = vec![
            ("profile", profile.to_string()),
            ("category", category.to_string()),
            ("asset_code", asset_code.to_string()),
            ("department", department.to_string()),
        ];
        match selector {
            VersionSelector::Current => {}
            VersionSelector::Latest => query.push(("latest", "true".to_string())),
            VersionSelector::Wip => bail!("wip versions are local-only and cannot be fetched"),
            VersionSelector::Version(version) => query.push(("version", version.0.to_string())),
        }
        self.get_json("/api/version", &query)
    }

    fn fetch_assets(
        &self,
        profile: &str,
        category: Option<&str>,
        asset_code: Option<&str>,
        department: Option<&str>,
    ) -> Result<AssetsResponse> {
        let mut query = vec![("profile", profile.to_string())];
        if let Some(category) = category {
            query.push(("category", category.to_string()));
        }
        if let Some(asset_code) = asset_code {
            query.push(("asset_code", asset_code.to_string()));
        }
        if let Some(department) = department {
            query.push(("department", department.to_string()));
        }
        self.get_json("/api/assets", &query)
    }

    fn fetch_versions(
        &self,
        profile: &str,
        category: &str,
        asset_code: &str,
        department: &str,
    ) -> Result<VersionsResponse> {
        let query = vec![
            ("profile", profile.to_string()),
            ("category", category.to_string()),
            ("asset_code", asset_code.to_string()),
            ("department", department.to_string()),
        ];
        self.get_json("/api/versions", &query)
    }

    fn fetch_current_status(
        &self,
        profile: &str,
        category: &str,
        asset_code: &str,
        department: &str,
    ) -> Result<CurrentStatus> {
        let query = vec![
            ("profile", profile.to_string()),
            ("category", category.to_string()),
            ("asset_code", asset_code.to_string()),
            ("department", department.to_string()),
        ];
        let statuses: Vec<CurrentStatus> = self.get_json("/api/current/status", &query)?;
        statuses
            .into_iter()
            .find(|status| {
                status.department_key.asset_key.category == category
                    && status.department_key.asset_key.asset_code == asset_code
                    && status.department_key.department == department
            })
            .ok_or_else(|| {
                anyhow!(
                    "remote current status not found for {}/{}/{}",
                    category,
                    asset_code,
                    department
                )
            })
    }

    fn fetch_object(&self, profile: &str, sha256: &str) -> Result<Vec<u8>> {
        validate_sha256(sha256)?;
        let query = vec![
            ("profile", profile.to_string()),
            ("sha256", sha256.to_string()),
        ];
        self.get_bytes("/api/object", &query)
    }

    fn object_status(
        &self,
        profile: &str,
        sha256: &str,
        expected_size: u64,
    ) -> Result<ObjectStatusResponse> {
        validate_sha256(sha256)?;
        let query = vec![
            ("profile", profile.to_string()),
            ("sha256", sha256.to_string()),
            ("size", expected_size.to_string()),
        ];
        self.get_json("/api/object/status", &query)
    }

    fn upload_object(
        &self,
        profile: &str,
        sha256: &str,
        bytes: &[u8],
    ) -> Result<ObjectUploadResponse> {
        validate_sha256(sha256)?;
        let query = vec![
            ("profile", profile.to_string()),
            ("sha256", sha256.to_string()),
        ];
        self.put_bytes_json("/api/object", &query, bytes)
    }

    fn import_version_info(
        &self,
        profile: &str,
        version_info: &VersionInfo,
    ) -> Result<VersionRecord> {
        self.put_json(
            "/api/version",
            &[],
            &VersionImportRequest {
                profile: profile.to_string(),
                version_info: version_info.clone(),
            },
        )
    }

    fn import_thumbnail_info(
        &self,
        profile: &str,
        thumbnail: &ThumbnailRecord,
    ) -> Result<ThumbnailRecord> {
        self.put_json(
            "/api/thumbnail",
            &[],
            &ThumbnailImportRequest {
                profile: profile.to_string(),
                thumbnail: thumbnail.clone(),
            },
        )
    }

    fn set_current_version(
        &self,
        profile: &str,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<CurrentStatus> {
        self.put_json(
            "/api/current",
            &[],
            &serde_json::json!({
                "profile": profile,
                "category": &department_key.asset_key.category,
                "asset_code": &department_key.asset_key.asset_code,
                "department": &department_key.department,
                "version": version,
            }),
        )
    }

    fn apply_current_status(&self, profile: &str, status: &CurrentStatus) -> Result<CurrentStatus> {
        if status.explicit {
            let version = status.current.ok_or_else(|| {
                anyhow!(
                    "local current status is explicit but has no current version: {}/{}/{}",
                    status.department_key.asset_key.category,
                    status.department_key.asset_key.asset_code,
                    status.department_key.department
                )
            })?;
            self.set_current_version(profile, &status.department_key, version)
        } else {
            self.put_json(
                "/api/current",
                &[],
                &serde_json::json!({
                    "profile": profile,
                    "category": &status.department_key.asset_key.category,
                    "asset_code": &status.department_key.asset_key.asset_code,
                    "department": &status.department_key.department,
                    "reset": true,
                }),
            )
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url(path, query);
        let response = self.request(&url)?;
        let status = response.status();
        let text = response
            .into_string()
            .with_context(|| format!("failed to read response from {url}"))?;
        if !(200..300).contains(&status) {
            bail!("remote request failed {status} {url}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("failed to decode JSON from {url}"))
    }

    fn put_json<T, B>(&self, path: &str, query: &[(&str, String)], body: &B) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize,
    {
        let url = self.url(path, query);
        let body = serde_json::to_vec(body).context("failed to encode JSON request")?;
        let response = self.put_request(&url, "application/json", &body)?;
        let status = response.status();
        let text = response
            .into_string()
            .with_context(|| format!("failed to read response from {url}"))?;
        if !(200..300).contains(&status) {
            bail!("remote request failed {status} {url}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("failed to decode JSON from {url}"))
    }

    fn get_bytes(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<u8>> {
        let url = self.url(path, query);
        let response = self.request(&url)?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let text = response.into_string().unwrap_or_default();
            bail!("remote request failed {status} {url}: {text}");
        }
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read response from {url}"))?;
        Ok(bytes)
    }

    fn put_bytes_json<T>(&self, path: &str, query: &[(&str, String)], body: &[u8]) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let url = self.url(path, query);
        let response = self.put_request(&url, "application/octet-stream", body)?;
        let status = response.status();
        let text = response
            .into_string()
            .with_context(|| format!("failed to read response from {url}"))?;
        if !(200..300).contains(&status) {
            bail!("remote request failed {status} {url}: {text}");
        }
        serde_json::from_str(&text).with_context(|| format!("failed to decode JSON from {url}"))
    }

    fn request(&self, url: &str) -> Result<ureq::Response> {
        match ureq::get(url)
            .set("Authorization", &format!("Bearer {}", self.auth_token))
            .call()
        {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(error) => Err(error).with_context(|| format!("remote request failed: {url}")),
        }
    }

    fn put_request(&self, url: &str, content_type: &str, body: &[u8]) -> Result<ureq::Response> {
        match ureq::put(url)
            .set("Authorization", &format!("Bearer {}", self.auth_token))
            .set("Content-Type", content_type)
            .send_bytes(body)
        {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(error) => Err(error).with_context(|| format!("remote request failed: {url}")),
        }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        let mut url = format!("{}/{}", self.server, path.trim_start_matches('/'));
        if !query.is_empty() {
            url.push('?');
            for (index, (key, value)) in query.iter().enumerate() {
                if index > 0 {
                    url.push('&');
                }
                url.push_str(&url_encode_component(key));
                url.push('=');
                url.push_str(&url_encode_component(value));
            }
        }
        url
    }
}

impl WebState {
    /// Opens every profile's store once and shares the handle across
    /// requests. RocksDB allows only a single open per store directory, so
    /// per-request opens would make concurrent API calls race on the LOCK
    /// file.
    fn try_new(config: ServeConfig) -> Result<Self> {
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
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
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

async fn serve_web(config: ServeConfig) -> Result<()> {
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

fn web_app(state: Arc<WebState>) -> Router {
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

async fn api_auth_middleware(
    State(state): State<Arc<WebState>>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, ApiError> {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let authorized = token
        .is_some_and(|token| constant_time_eq(token.as_bytes(), state.auth_token.as_bytes()));
    if authorized {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::unauthorized("missing or invalid bearer token"))
    }
}

/// Constant-time byte comparison for the bearer-token check, so response
/// timing does not leak how much of a guessed token matched. The length
/// itself is not treated as secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

async fn index_html() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
        .into_response()
}

async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

async fn api_profiles(
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

async fn api_assets(
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

async fn api_asset(
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

async fn api_versions(
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

async fn api_version_info(
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

async fn api_import_version(
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

async fn api_object_status(
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

async fn api_object(
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

async fn api_upload_object(
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

async fn api_current_status(
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

async fn api_update_current(
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

async fn api_materialize(
    State(state): State<Arc<WebState>>,
    Json(request): Json<WorkspacePullRequest>,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    api_pull_impl(state, request).await
}

async fn api_pull(
    State(state): State<Arc<WebState>>,
    Json(request): Json<WorkspacePullRequest>,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    api_pull_impl(state, request).await
}

async fn api_restore(
    State(state): State<Arc<WebState>>,
    Json(request): Json<WorkspacePullRequest>,
) -> std::result::Result<Json<MaterializeOutcome>, ApiError> {
    if request.version.is_none() {
        return Err(ApiError::bad_request("version is required for restore"));
    }
    api_pull_impl(state, request).await
}

async fn api_pull_impl(
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

async fn api_upload_thumbnail(
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

async fn api_import_thumbnail(
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

async fn api_thumbnail_url(
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

async fn api_resolve(
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

#[derive(Debug)]
struct ThumbnailUpload {
    profile: String,
    category: String,
    asset_code: String,
    department: String,
    version: VersionId,
    bytes: Vec<u8>,
}

async fn parse_thumbnail_upload(
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

async fn read_multipart_text(
    field: axum::extract::multipart::Field<'_>,
) -> std::result::Result<String, ApiError> {
    field
        .text()
        .await
        .map(|value| value.trim().to_string())
        .map_err(|error| ApiError::bad_request(error.to_string()))
}

fn required_upload_field(
    value: Option<String>,
    field: &str,
) -> std::result::Result<String, ApiError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request(format!("missing multipart field {field}")))
}

async fn run_store_read<T, F>(f: F) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))
}

async fn run_store_write<T, F>(lock: Arc<Mutex<()>>, f: F) -> std::result::Result<T, ApiError>
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

fn build_asset_cards(store: &Store, query: &AssetsQuery) -> Result<AssetsResponse> {
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
        let thumbnail_url = status.current.and_then(|version| {
            store
                .thumbnail_url(&department_key, VersionSelector::Version(version), None)
                .ok()
        });

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

fn profile_for(state: &WebState, name: &str) -> std::result::Result<WebProfile, ApiError> {
    state
        .profiles
        .get(name)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("profile not found: {name}")))
}

fn parse_resolve_mode(value: &str) -> Result<ResolveMode> {
    match value {
        "auto" => Ok(ResolveMode::Auto),
        "local" => Ok(ResolveMode::Local),
        "remote" => Ok(ResolveMode::Remote),
        _ => bail!("resolve mode must be auto, local, or remote: {value}"),
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn validate_profile_name(value: &str) -> Result<()> {
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

fn unique_temp_upload_path() -> PathBuf {
    let nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp_micros());
    std::env::temp_dir().join(format!(
        "ads-thumbnail-{}-{nanos}.upload",
        std::process::id()
    ))
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ADS Asset Browser</title>
  <link rel="stylesheet" href="/style.css">
</head>
<body>
  <div id="auth" class="auth hidden">
    <form id="authForm" class="auth-card">
      <div class="auth-mark">ADS</div>
      <h1>ADS Asset Browser</h1>
      <p class="auth-note">Production asset store &mdash; authorization required</p>
      <input id="tokenInput" type="password" autocomplete="current-password" placeholder="Bearer token" spellcheck="false">
      <button type="submit" class="solid-btn">Unlock</button>
    </form>
  </div>

  <div class="shell">
    <header class="slate">
      <div class="brand">
        <span class="brand-badge">ADS</span>
        <span class="brand-sub">Asset&nbsp;Browser</span>
      </div>
      <label class="slate-field">
        <span class="microlabel">Profile</span>
        <select id="profileSelect"></select>
      </label>
      <div class="slate-search">
        <input id="searchInput" type="search" placeholder="Search assets, categories, departments&hellip;" spellcheck="false">
      </div>
      <div class="slate-right">
        <span class="schema-chip">SCHEMA&nbsp;V8</span>
        <span id="connectionLed" class="led" title="Connection"></span>
        <button id="refreshButton" type="button" class="ghost-btn">Rescan</button>
        <button id="logoutButton" type="button" class="ghost-btn">Lock</button>
      </div>
    </header>

    <aside class="rail">
      <nav class="rail-block">
        <h2 class="microlabel">Category</h2>
        <ul id="categoryList" class="rail-list"></ul>
      </nav>
      <nav class="rail-block">
        <h2 class="microlabel">Department</h2>
        <ul id="departmentList" class="rail-list"></ul>
      </nav>
      <div class="rail-foot">
        <div id="status" class="status"></div>
      </div>
    </aside>

    <main class="stage">
      <div class="stage-bar">
        <span id="assetCount" class="count">0 ASSETS</span>
      </div>
      <div id="assetGrid" class="grid"></div>
      <div id="gridEmpty" class="void hidden">
        <div class="void-cube"></div>
        <p>No assets match the current filters.</p>
      </div>
    </main>

    <aside class="inspector">
      <div id="detailEmpty" class="void">
        <div class="void-cube"></div>
        <p>Select an asset to inspect</p>
      </div>
      <div id="detailPanel" class="inspector-body hidden">
        <div class="inspector-head">
          <div class="inspector-title">
            <h2 id="detailTitle"></h2>
            <p id="detailSubtitle" class="mono"></p>
          </div>
          <span id="detailDepartment" class="dept-tag"></span>
        </div>
        <div id="detailPreview" class="preview"></div>

        <section class="inspector-section">
          <h3 class="microlabel">ADS URI</h3>
          <div class="uri-row">
            <input id="assetUriInput" type="text" readonly spellcheck="false">
            <button id="copyAssetUriButton" type="button" class="amber-btn">Copy</button>
          </div>
        </section>

        <section class="inspector-section">
          <h3 class="microlabel">Version</h3>
          <div class="version-controls">
            <select id="versionSelect"></select>
            <label class="force-check"><input id="forcePull" type="checkbox"><span>Force</span></label>
          </div>
          <div class="action-row">
            <button id="setCurrentButton" type="button" class="amber-btn">Pin Current</button>
            <button id="resetCurrentButton" type="button" class="ghost-btn">Reset</button>
          </div>
          <button id="pullButton" type="button" class="solid-btn block-btn">Pull to Workspace</button>
        </section>

        <section class="inspector-section">
          <h3 class="microlabel">Take Log</h3>
          <div id="versionList" class="take-log"></div>
        </section>

        <section class="inspector-section">
          <div class="section-head">
            <h3 class="microlabel">Manifest</h3>
            <span id="manifestSummary" class="manifest-summary"></span>
          </div>
          <div id="manifestList" class="manifest"></div>
        </section>

        <section class="inspector-section">
          <h3 class="microlabel">Thumbnail</h3>
          <div class="action-row">
            <label class="ghost-btn upload-btn">Upload<input id="thumbnailInput" type="file" accept="image/png,image/jpeg,image/webp"></label>
            <button id="copyThumbUrlButton" type="button" class="ghost-btn">Copy URL</button>
          </div>
        </section>
      </div>
    </aside>
  </div>
  <script src="/app.js"></script>
</body>
</html>
"#;

const STYLE_CSS: &str = r#":root {
  color-scheme: dark;
  --bg: #131210;
  --panel: #1a1816;
  --panel-2: #201d1a;
  --panel-3: #272320;
  --line: #2c2823;
  --line-strong: #3d372f;
  --ink: #e9e4da;
  --ink-dim: #9b948a;
  --ink-faint: #6e6759;
  --amber: #f2a93c;
  --amber-bright: #ffc14d;
  --amber-dim: rgba(242, 169, 60, .13);
  --green: #8cd97c;
  --red: #e0604a;
  --mono: 'Cascadia Code', Consolas, 'SF Mono', 'JetBrains Mono', monospace;
  --sans: Bahnschrift, 'Avenir Next Condensed', 'Segoe UI', 'Helvetica Neue', sans-serif;
  --radius: 4px;
}

* { box-sizing: border-box; }

html, body { height: 100%; }

body {
  margin: 0;
  font-family: var(--sans);
  font-size: 14px;
  color: var(--ink);
  background:
    radial-gradient(1100px 700px at 75% -12%, rgba(242, 169, 60, .045), transparent 60%),
    repeating-linear-gradient(0deg, rgba(255, 255, 255, .012) 0 1px, transparent 1px 3px),
    var(--bg);
  overflow: hidden;
}

::selection { background: var(--amber-dim); color: var(--amber-bright); }

:focus-visible { outline: 1px solid var(--amber); outline-offset: 1px; }

::-webkit-scrollbar { width: 10px; height: 10px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb { background: var(--line-strong); border-radius: 5px; border: 2px solid var(--bg); }
::-webkit-scrollbar-thumb:hover { background: var(--ink-faint); }

.hidden { display: none !important; }

.mono { font-family: var(--mono); }

.microlabel {
  margin: 0;
  font-size: 10px;
  font-weight: 600;
  letter-spacing: .16em;
  text-transform: uppercase;
  color: var(--ink-faint);
}

/* ---------- shell layout ---------- */

.shell {
  display: grid;
  grid-template-columns: 218px 1fr 348px;
  grid-template-rows: 52px 1fr;
  grid-template-areas:
    'slate slate slate'
    'rail  stage inspector';
  height: 100vh;
}

@media (max-width: 1280px) {
  .shell { grid-template-columns: 196px 1fr 312px; }
}

/* ---------- top slate ---------- */

.slate {
  grid-area: slate;
  display: flex;
  align-items: center;
  gap: 18px;
  padding: 0 16px;
  background: linear-gradient(180deg, var(--panel-2), var(--panel));
  border-bottom: 1px solid var(--line-strong);
}

.brand { display: flex; align-items: baseline; gap: 10px; }

.brand-badge {
  display: inline-block;
  padding: 3px 9px 2px;
  background: var(--amber);
  color: #181307;
  font-weight: 700;
  font-size: 15px;
  letter-spacing: .22em;
  border-radius: 2px;
  transform: translateY(-1px);
}

.brand-sub {
  font-size: 11px;
  letter-spacing: .24em;
  text-transform: uppercase;
  color: var(--ink-dim);
}

.slate-field { display: flex; align-items: center; gap: 8px; }

.slate-field select {
  min-width: 130px;
}

.slate-search { flex: 1; max-width: 520px; }

.slate-search input {
  width: 100%;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 7px 12px;
  color: var(--ink);
  font-family: var(--mono);
  font-size: 12.5px;
  transition: border-color .15s ease;
}

.slate-search input:hover { border-color: var(--line-strong); }
.slate-search input:focus { border-color: var(--amber); outline: none; }

.slate-right { display: flex; align-items: center; gap: 10px; margin-left: auto; }

.schema-chip {
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: .1em;
  color: var(--ink-faint);
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 3px 9px;
}

.led {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: var(--ink-faint);
  box-shadow: 0 0 0 0 transparent;
  transition: background .2s ease;
}

.led.on {
  background: var(--green);
  animation: pulse 2.4s ease-in-out infinite;
}

.led.err { background: var(--red); animation: none; }

@keyframes pulse {
  0%, 100% { box-shadow: 0 0 0 0 rgba(140, 217, 124, .35); }
  50% { box-shadow: 0 0 7px 2px rgba(140, 217, 124, .25); }
}

/* ---------- controls ---------- */

select {
  appearance: none;
  background: var(--panel-3);
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  color: var(--ink);
  font-family: var(--mono);
  font-size: 12.5px;
  padding: 6px 26px 6px 10px;
  background-image: linear-gradient(45deg, transparent 50%, var(--ink-dim) 50%),
    linear-gradient(135deg, var(--ink-dim) 50%, transparent 50%);
  background-position: calc(100% - 15px) 55%, calc(100% - 10px) 55%;
  background-size: 5px 5px;
  background-repeat: no-repeat;
  cursor: pointer;
  transition: border-color .15s ease;
}

select:hover { border-color: var(--ink-faint); }
select:focus { border-color: var(--amber); outline: none; }

button { font-family: var(--sans); cursor: pointer; }

.ghost-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  background: transparent;
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  color: var(--ink-dim);
  font-size: 12px;
  letter-spacing: .06em;
  padding: 6px 12px;
  transition: color .15s ease, border-color .15s ease, background .15s ease;
}

.ghost-btn:hover { color: var(--ink); border-color: var(--ink-faint); background: var(--panel-2); }

.amber-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  background: transparent;
  border: 1px solid var(--amber);
  border-radius: var(--radius);
  color: var(--amber);
  font-size: 12px;
  font-weight: 600;
  letter-spacing: .06em;
  padding: 6px 12px;
  transition: background .15s ease, color .15s ease;
}

.amber-btn:hover { background: var(--amber-dim); color: var(--amber-bright); }

.solid-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  background: var(--amber);
  border: 1px solid var(--amber);
  border-radius: var(--radius);
  color: #181307;
  font-size: 12.5px;
  font-weight: 700;
  letter-spacing: .08em;
  text-transform: uppercase;
  padding: 8px 14px;
  transition: background .15s ease, transform .1s ease;
}

.solid-btn:hover { background: var(--amber-bright); }
.solid-btn:active { transform: translateY(1px); }

.block-btn { width: 100%; margin-top: 8px; }

/* ---------- left rail ---------- */

.rail {
  grid-area: rail;
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 16px 12px 12px;
  background: var(--panel);
  border-right: 1px solid var(--line);
  overflow-y: auto;
}

.rail-block .microlabel { padding: 0 6px 8px; }

.rail-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 1px; }

.rail-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 6px 9px;
  border-radius: 3px;
  border-left: 2px solid transparent;
  color: var(--ink-dim);
  font-size: 13px;
  cursor: pointer;
  transition: background .12s ease, color .12s ease;
}

.rail-item:hover { background: var(--panel-2); color: var(--ink); }

.rail-item.active {
  background: var(--amber-dim);
  border-left-color: var(--amber);
  color: var(--amber-bright);
}

.rail-label {
  display: flex;
  align-items: center;
  gap: 7px;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.dept-dot {
  flex-shrink: 0;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: hsl(var(--dh) 50% 58%);
  box-shadow: 0 0 4px hsl(var(--dh) 50% 58% / .4);
}

.rail-item .n {
  font-family: var(--mono);
  font-size: 10.5px;
  color: var(--ink-faint);
}

.rail-item.active .n { color: var(--amber); }

.rail-foot { margin-top: auto; padding: 6px; }

.status {
  min-height: 16px;
  font-family: var(--mono);
  font-size: 11px;
  line-height: 1.5;
  color: var(--ink-faint);
  word-break: break-word;
}

.status.ok { color: var(--green); }
.status.err { color: var(--red); }

/* ---------- stage / asset grid ---------- */

.stage {
  grid-area: stage;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.stage-bar {
  display: flex;
  align-items: center;
  padding: 12px 18px 4px;
}

.count {
  font-family: var(--mono);
  font-size: 11px;
  letter-spacing: .14em;
  color: var(--ink-faint);
}

.grid {
  flex: 1;
  overflow-y: auto;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(168px, 1fr));
  /* Explicit row sizing: Chromium collapses `auto` rows to near-zero in
     scrollable grids with thousands of rows. */
  grid-auto-rows: max-content;
  gap: 12px;
  align-content: start;
  padding: 12px 18px 24px;
}

.card {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  overflow: hidden;
  cursor: pointer;
  animation: rise .4s ease backwards;
  transition: transform .15s ease, border-color .15s ease, box-shadow .15s ease;
}

.card:hover {
  transform: translateY(-2px);
  border-color: var(--line-strong);
  box-shadow: 0 8px 20px rgba(0, 0, 0, .45);
}

.card.active {
  border-color: var(--amber);
  box-shadow: 0 0 0 1px var(--amber), 0 10px 24px rgba(0, 0, 0, .5);
}

.card:nth-child(1) { animation-delay: .02s; }
.card:nth-child(2) { animation-delay: .05s; }
.card:nth-child(3) { animation-delay: .08s; }
.card:nth-child(4) { animation-delay: .11s; }
.card:nth-child(5) { animation-delay: .14s; }
.card:nth-child(6) { animation-delay: .17s; }
.card:nth-child(7) { animation-delay: .2s; }
.card:nth-child(8) { animation-delay: .23s; }
.card:nth-child(9) { animation-delay: .26s; }
.card:nth-child(10) { animation-delay: .29s; }
.card:nth-child(11) { animation-delay: .32s; }
.card:nth-child(12) { animation-delay: .35s; }

@keyframes rise {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: none; }
}

.thumb {
  position: relative;
  aspect-ratio: 4 / 3;
  background:
    url('data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 80 80%22%3E%3Cg fill=%22none%22 stroke=%22%23332e28%22 stroke-width=%221%22%3E%3Cpath d=%22M40 14 66 28v24L40 66 14 52V28z%22/%3E%3Cpath d=%22M40 14v24m0 0L14 28m26 10 26-10M40 38v28%22/%3E%3C/g%3E%3C/svg%3E') center / 64px no-repeat,
    linear-gradient(160deg, var(--panel-2), var(--panel));
  border-bottom: 1px solid var(--line);
}

.thumb img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
}

.dept-badge {
  position: absolute;
  top: 6px;
  left: 6px;
  max-width: calc(100% - 12px);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: var(--mono);
  font-size: 9px;
  font-weight: 600;
  letter-spacing: .1em;
  text-transform: uppercase;
  padding: 2px 7px;
  border-radius: 2px;
  background: rgba(12, 11, 9, .78);
  border: 1px solid hsl(var(--dh) 45% 60% / .55);
  color: hsl(var(--dh) 55% 70%);
  pointer-events: none;
}

.card-meta { padding: 9px 10px 10px; display: flex; flex-direction: column; gap: 4px; }

.card-name {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
  font-weight: 600;
  font-size: 13.5px;
  letter-spacing: .02em;
}

.card-name .ver {
  font-family: var(--mono);
  font-size: 11px;
  font-weight: 400;
  color: var(--amber);
}

.card-sub {
  font-size: 11px;
  color: var(--ink-faint);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ---------- empty states ---------- */

.void {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  color: var(--ink-faint);
  font-size: 12.5px;
  letter-spacing: .04em;
  padding: 32px;
  text-align: center;
}

.void-cube {
  width: 72px;
  height: 72px;
  background: url('data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 80 80%22%3E%3Cg fill=%22none%22 stroke=%22%233d372f%22 stroke-width=%221.2%22%3E%3Cpath d=%22M40 10 70 26v28L40 70 10 54V26z%22/%3E%3Cpath d=%22M40 10v28m0 0L10 26m30 12 30-12M40 38v32%22/%3E%3C/g%3E%3C/svg%3E') center / contain no-repeat;
  opacity: .9;
}

/* ---------- inspector ---------- */

.inspector {
  grid-area: inspector;
  display: flex;
  flex-direction: column;
  background: var(--panel);
  border-left: 1px solid var(--line);
  overflow-y: auto;
}

.inspector-body {
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 18px 16px 24px;
  animation: rise .3s ease backwards;
}

.inspector-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 10px;
}

.inspector-title h2 {
  margin: 0;
  font-size: 19px;
  font-weight: 700;
  letter-spacing: .02em;
}

.inspector-title p {
  margin: 3px 0 0;
  font-size: 11px;
  color: var(--ink-faint);
}

.dept-tag {
  flex-shrink: 0;
  font-family: var(--mono);
  font-size: 10.5px;
  letter-spacing: .08em;
  text-transform: uppercase;
  color: hsl(var(--dh, 38) 55% 70%);
  background: hsl(var(--dh, 38) 45% 60% / .1);
  border: 1px solid hsl(var(--dh, 38) 45% 60% / .5);
  border-radius: 2px;
  padding: 3px 8px;
}

.preview {
  position: relative;
  aspect-ratio: 16 / 10;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  overflow: hidden;
  background:
    url('data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 80 80%22%3E%3Cg fill=%22none%22 stroke=%22%23332e28%22 stroke-width=%221%22%3E%3Cpath d=%22M40 14 66 28v24L40 66 14 52V28z%22/%3E%3Cpath d=%22M40 14v24m0 0L14 28m26 10 26-10M40 38v28%22/%3E%3C/g%3E%3C/svg%3E') center / 72px no-repeat,
    linear-gradient(160deg, var(--panel-2), var(--bg));
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--ink-faint);
  font-size: 11.5px;
  letter-spacing: .06em;
}

.preview img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
}

.inspector-section { display: flex; flex-direction: column; gap: 9px; }

.uri-row { display: flex; gap: 8px; }

.uri-row input {
  flex: 1;
  min-width: 0;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  color: var(--amber-bright);
  font-family: var(--mono);
  font-size: 11.5px;
  padding: 7px 10px;
}

.uri-row input:focus { border-color: var(--amber); outline: none; }

.version-controls { display: flex; align-items: center; gap: 10px; }

.version-controls select { flex: 1; }

.force-check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--ink-dim);
  cursor: pointer;
  user-select: none;
}

.force-check input { accent-color: var(--amber); }

.action-row { display: flex; gap: 8px; }

.action-row > * { flex: 1; }

.upload-btn { position: relative; overflow: hidden; }

.upload-btn input { position: absolute; inset: 0; opacity: 0; cursor: pointer; }

/* ---------- take log ---------- */

.take-log {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  overflow: hidden;
  max-height: 300px;
  overflow-y: auto;
}

.take {
  display: grid;
  grid-template-columns: auto 1fr auto;
  align-items: center;
  gap: 10px;
  padding: 7px 10px;
  border-left: 2px solid transparent;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
  cursor: pointer;
  transition: background .12s ease;
}

.take:last-child { border-bottom: none; }

.take:hover { background: var(--panel-2); }

.take.selected { background: var(--panel-3); }

.take.current { border-left-color: var(--amber); background: var(--amber-dim); }

.take .v {
  font-family: var(--mono);
  font-size: 12px;
  font-weight: 600;
  color: var(--ink);
}

.take.current .v { color: var(--amber-bright); }

.take .meta {
  font-family: var(--mono);
  font-size: 10.5px;
  color: var(--ink-faint);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.take .tag {
  font-family: var(--mono);
  font-size: 9px;
  font-weight: 700;
  letter-spacing: .12em;
  padding: 2px 6px;
  border-radius: 2px;
}

.take .tag.pin { background: var(--amber); color: #181307; }

.take .tag.latest { border: 1px solid var(--line-strong); color: var(--ink-faint); }

/* ---------- manifest ---------- */

.section-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}

.manifest-summary {
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: .06em;
  color: var(--ink-faint);
}

.manifest {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  max-height: 260px;
  overflow-y: auto;
}

.file-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
  cursor: pointer;
  transition: background .12s ease;
}

.file-row:last-child { border-bottom: none; }

.file-row:hover { background: var(--panel-2); }

.file-row:hover .file-path { color: var(--amber-bright); }

.file-dot {
  flex-shrink: 0;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: hsl(var(--dh) 50% 58%);
}

.file-dot.neutral { background: var(--line-strong); }

.file-path {
  flex: 1;
  min-width: 0;
  font-family: var(--mono);
  font-size: 11px;
  color: var(--ink-dim);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  transition: color .12s ease;
}

.file-size {
  flex-shrink: 0;
  font-family: var(--mono);
  font-size: 10px;
  color: var(--ink-faint);
}

/* ---------- auth gate ---------- */

.auth {
  position: fixed;
  inset: 0;
  z-index: 50;
  display: flex;
  align-items: center;
  justify-content: center;
  background:
    radial-gradient(900px 600px at 50% 0%, rgba(242, 169, 60, .05), transparent 60%),
    rgba(12, 11, 10, .92);
  backdrop-filter: blur(6px);
}

.auth-card {
  display: flex;
  flex-direction: column;
  gap: 14px;
  width: min(360px, calc(100vw - 48px));
  padding: 30px 28px 28px;
  background: var(--panel);
  border: 1px solid var(--line-strong);
  border-top: 2px solid var(--amber);
  border-radius: 6px;
  box-shadow: 0 24px 60px rgba(0, 0, 0, .6);
  animation: rise .35s ease backwards;
}

.auth-mark {
  align-self: flex-start;
  padding: 4px 12px 3px;
  background: var(--amber);
  color: #181307;
  font-weight: 700;
  font-size: 18px;
  letter-spacing: .26em;
  border-radius: 2px;
}

.auth-card h1 { margin: 2px 0 0; font-size: 17px; font-weight: 600; letter-spacing: .03em; }

.auth-note {
  margin: 0;
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: .12em;
  text-transform: uppercase;
  color: var(--ink-faint);
}

.auth-card input {
  background: var(--bg);
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  color: var(--ink);
  font-family: var(--mono);
  font-size: 13px;
  padding: 9px 12px;
}

.auth-card input:focus { border-color: var(--amber); outline: none; }
"#;

const APP_JS: &str = r#"const state = {
  token: sessionStorage.getItem('adsToken') || '',
  profile: '',
  allAssets: [],
  assets: [],
  category: '',
  department: '',
  selected: null,
  versions: [],
  selectedVersion: '',
  manifestCache: new Map()
};

const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
// Versions are plain integers on the wire (schema v8); v### is display-only sugar.
const fmtVersion = (version) => (version === null || version === undefined || version === '') ? '-' : 'v' + String(version).padStart(3, '0');

// Department badge hues: fixed palette for the canonical departments, stable
// hash fallback for custom names. The amber zone (~38) is reserved for the
// app accent, so fx sits at red instead.
const DEPT_HUES = {model: 210, lookdev: 268, texture: 175, textures: 175, tex: 175, rig: 330, anim: 110, layout: 75, fx: 8};
function deptHue(name) {
  const key = String(name || '').toLowerCase();
  if (key in DEPT_HUES) return DEPT_HUES[key];
  let hash = 0;
  for (const ch of key) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  return hash % 360;
}

let statusTimer = null;
function status(text, kind) {
  const node = $('status');
  node.textContent = text || '';
  node.className = 'status' + (kind ? ' ' + kind : '');
  clearTimeout(statusTimer);
  if (text && kind === 'ok') {
    statusTimer = setTimeout(() => { node.textContent = ''; node.className = 'status'; }, 4000);
  }
}

function led(stateName) {
  $('connectionLed').className = 'led' + (stateName ? ' ' + stateName : '');
}

function humanBytes(bytes) {
  if (!Number.isFinite(bytes)) return '-';
  if (bytes < 1024) return bytes + ' B';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = '';
  for (const next of units) {
    value /= 1024;
    unit = next;
    if (value < 1024) break;
  }
  return value.toFixed(value < 10 ? 1 : 0) + ' ' + unit;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (_) {
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.left = '-9999px';
    document.body.appendChild(area);
    area.focus();
    area.select();
    area.setSelectionRange(0, area.value.length);
    const ok = document.execCommand('copy');
    document.body.removeChild(area);
    return ok;
  }
}

async function api(path, options = {}) {
  const headers = options.headers ? new Headers(options.headers) : new Headers();
  headers.set('Authorization', `Bearer ${state.token}`);
  if (options.body && !(options.body instanceof FormData)) headers.set('Content-Type', 'application/json');
  const res = await fetch(path, {...options, headers});
  if (res.status === 401) {
    sessionStorage.removeItem('adsToken');
    led('err');
    $('auth').classList.remove('hidden');
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try { message = (await res.json()).error || message; } catch (_) {}
    throw new Error(message);
  }
  return res.json();
}

function qs(params) {
  const search = new URLSearchParams();
  Object.entries(params).forEach(([key, value]) => {
    if (value !== undefined && value !== null && value !== '') search.set(key, value);
  });
  return search.toString();
}

async function init() {
  if (!state.token) {
    $('auth').classList.remove('hidden');
    led('');
    return;
  }
  $('auth').classList.add('hidden');
  const data = await api('/api/profiles');
  const select = $('profileSelect');
  select.innerHTML = data.profiles.map(p => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join('');
  state.profile = select.value || data.profiles[0]?.name || '';
  led('on');
  await loadAssets();
}

async function loadAssets() {
  if (!state.profile) return;
  status('Scanning store…');
  const params = {profile: state.profile, q: $('searchInput').value};
  const data = await api(`/api/assets?${qs(params)}`);
  state.allAssets = data.assets;
  status('');
  applyFilters();
}

function applyFilters() {
  if (state.category && !state.allAssets.some(a => a.category === state.category)) state.category = '';
  if (state.department && !state.allAssets.some(a => a.department === state.department)) state.department = '';
  state.assets = state.allAssets.filter(a =>
    (!state.category || a.category === state.category) &&
    (!state.department || a.department === state.department));
  renderRails();
  renderGrid();
}

function railItems(values, selected, withDot) {
  const counts = new Map();
  values.forEach(v => counts.set(v, (counts.get(v) || 0) + 1));
  const keys = [...counts.keys()].sort();
  const label = (key) => withDot
    ? `<span class="rail-label"><span class="dept-dot" style="--dh:${deptHue(key)}"></span>${esc(key)}</span>`
    : `<span class="rail-label">${esc(key)}</span>`;
  const all = `<li class="rail-item${selected === '' ? ' active' : ''}" data-value=""><span class="rail-label">All</span><span class="n">${values.length}</span></li>`;
  return all + keys.map(key =>
    `<li class="rail-item${selected === key ? ' active' : ''}" data-value="${esc(key)}">${label(key)}<span class="n">${counts.get(key)}</span></li>`
  ).join('');
}

function renderRails() {
  $('categoryList').innerHTML = railItems(state.allAssets.map(a => a.category), state.category, false);
  $('departmentList').innerHTML = railItems(state.allAssets.map(a => a.department), state.department, true);
  $('categoryList').querySelectorAll('.rail-item').forEach(item => {
    item.addEventListener('click', () => { state.category = item.dataset.value; applyFilters(); });
  });
  $('departmentList').querySelectorAll('.rail-item').forEach(item => {
    item.addEventListener('click', () => { state.department = item.dataset.value; applyFilters(); });
  });
}

function renderGrid() {
  const count = state.assets.length;
  $('assetCount').textContent = `${count} ASSET${count === 1 ? '' : 'S'}`;
  $('gridEmpty').classList.toggle('hidden', count > 0);
  $('assetGrid').innerHTML = state.assets.map(asset => {
    const key = assetKey(asset);
    const active = state.selected && assetKey(state.selected) === key ? ' active' : '';
    const thumb = asset.thumbnail_url ? `<img src="${esc(asset.thumbnail_url)}" alt="" loading="lazy" onerror="this.remove()">` : '';
    return `<article class="card${active}" data-key="${esc(key)}">
      <div class="thumb">${thumb}<span class="dept-badge" style="--dh:${deptHue(asset.department)}">${esc(asset.department)}</span></div>
      <div class="card-meta">
        <div class="card-name"><span>${esc(asset.asset_code)}</span><span class="ver">${fmtVersion(asset.current)}</span></div>
        <div class="card-sub">${esc(asset.category)}</div>
        <div class="card-sub">${asset.version_count} versions · latest ${fmtVersion(asset.latest)}</div>
      </div>
    </article>`;
  }).join('');
  document.querySelectorAll('.card').forEach(card => {
    card.addEventListener('click', () => {
      const asset = state.assets.find(item => assetKey(item) === card.dataset.key);
      if (asset) selectAsset(asset).catch(showError);
    });
  });
}

function setActiveCard(key) {
  document.querySelectorAll('.card.active').forEach(card => card.classList.remove('active'));
  const card = document.querySelector(`.card[data-key="${CSS.escape(key)}"]`);
  if (card) card.classList.add('active');
}

async function selectAsset(asset) {
  state.selected = asset;
  state.selectedVersion = asset.current || asset.latest || '';
  // Class swap instead of re-rendering the grid: with thousands of cards a
  // full rebuild costs ~80ms per click and reloads thumbnails.
  setActiveCard(assetKey(asset));
  const params = {profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department};
  const data = await api(`/api/versions?${qs(params)}`);
  state.versions = data.versions;
  state.lastCurrentStatus = data.current_status;
  renderDetail(asset, data);
  await loadManifest($('versionSelect').value);
}

function renderDetail(asset, data) {
  $('detailEmpty').classList.add('hidden');
  $('detailPanel').classList.remove('hidden');
  $('detailTitle').textContent = asset.asset_code;
  $('detailSubtitle').textContent = asset.category;
  $('detailDepartment').textContent = asset.department;
  $('detailDepartment').style.setProperty('--dh', deptHue(asset.department));
  $('detailPreview').innerHTML = asset.thumbnail_url ? `<img src="${esc(asset.thumbnail_url)}" alt="" onerror="this.remove()">` : '<span>NO THUMBNAIL</span>';
  const options = data.versions.map(v => `<option value="${esc(v.version)}">${fmtVersion(v.version)}</option>`).join('');
  $('versionSelect').innerHTML = options;
  $('versionSelect').value = String(state.selectedVersion || data.current_status.current || data.current_status.latest || '');
  updateAssetUriField();
  renderTakeLog(data);
}

function renderTakeLog(data) {
  const selected = $('versionSelect').value;
  const currentStatus = data.current_status || {};
  $('versionList').innerHTML = data.versions.slice().reverse().map(v => {
    const isCurrent = v.version === currentStatus.current;
    const isLatest = v.version === currentStatus.latest;
    const isSelected = String(v.version) === selected;
    const date = (v.created_at || '').slice(0, 10);
    const tag = isCurrent
      ? '<span class="tag pin">PIN</span>'
      : (isLatest ? '<span class="tag latest">LATEST</span>' : '');
    return `<div class="take${isCurrent ? ' current' : ''}${isSelected ? ' selected' : ''}" data-version="${esc(v.version)}">
      <span class="v">${fmtVersion(v.version)}</span>
      <span class="meta">${v.file_count} files · ${humanBytes(v.total_bytes)}${date ? ' · ' + esc(date) : ''}</span>
      ${tag}
    </div>`;
  }).join('');
  $('versionList').querySelectorAll('.take').forEach(row => {
    row.addEventListener('click', () => {
      $('versionSelect').value = row.dataset.version;
      state.selectedVersion = row.dataset.version;
      updateAssetUriField();
      renderTakeLog(data);
      loadManifest(row.dataset.version).catch(showError);
    });
  });
}

const USD_EXTENSIONS = ['usd', 'usda', 'usdc', 'usdz'];
const TEXTURE_EXTENSIONS = ['tx', 'rat', 'exr', 'tif', 'tiff', 'png', 'jpg', 'jpeg', 'tga', 'bmp', 'hdr', 'pic', 'tex'];

function fileKindHue(path) {
  const ext = String(path).split('.').pop().toLowerCase();
  if (USD_EXTENSIONS.includes(ext)) return 38;
  if (TEXTURE_EXTENSIONS.includes(ext)) return 175;
  return null;
}

async function loadManifest(version) {
  const asset = state.selected;
  if (!asset || version === '' || version === null || version === undefined) return;
  const cacheKey = `${state.profile}/${assetKey(asset)}@${version}`;
  let info = state.manifestCache.get(cacheKey);
  if (!info) {
    const params = {profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version};
    info = await api(`/api/version?${qs(params)}`);
    state.manifestCache.set(cacheKey, info);
  }
  renderManifest(info, version);
}

function renderManifest(info, version) {
  const entries = (info.manifest && info.manifest.entries) || [];
  const total = entries.reduce((sum, entry) => sum + entry.size, 0);
  $('manifestSummary').textContent = entries.length
    ? `${entries.length} file${entries.length === 1 ? '' : 's'} · ${humanBytes(total)}`
    : 'empty';
  $('manifestList').innerHTML = entries.map(entry => {
    const hue = fileKindHue(entry.relative_path);
    const dot = hue === null
      ? '<span class="file-dot neutral"></span>'
      : `<span class="file-dot" style="--dh:${hue}"></span>`;
    return `<div class="file-row" data-path="${esc(entry.relative_path)}" title="sha256 ${esc(entry.sha256)} — click to copy ads:// URI">
      ${dot}
      <span class="file-path">${esc(entry.relative_path)}</span>
      <span class="file-size">${humanBytes(entry.size)}</span>
    </div>`;
  }).join('');
  $('manifestList').querySelectorAll('.file-row').forEach(row => {
    row.addEventListener('click', async () => {
      const asset = state.selected;
      if (!asset) return;
      const uri = `ads://${asset.category}/${asset.asset_code}/${asset.department}/${row.dataset.path}?v=${encodeURIComponent(version)}`;
      const copied = await copyText(uri);
      status(copied ? `Copied ${uri}` : `Clipboard blocked. ${uri}`, copied ? 'ok' : 'err');
    });
  });
}

async function refreshSelection() {
  await loadAssets();
  const asset = state.selected;
  if (!asset) return;
  const refreshed = state.assets.find(item => assetKey(item) === assetKey(asset));
  if (refreshed) await selectAsset(refreshed);
}

async function setCurrent() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  await api('/api/current', {method: 'PUT', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version})});
  await refreshSelection();
  status(`Pinned current to ${fmtVersion(version)}`, 'ok');
}

async function resetCurrent() {
  const asset = state.selected;
  if (!asset) return;
  await api('/api/current', {method: 'PUT', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, reset: true})});
  await refreshSelection();
  status('Current follows latest', 'ok');
}

async function pullToWorkspace() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  status('Pulling…');
  await api('/api/pull', {method: 'POST', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version, force: $('forcePull').checked})});
  status(`Pulled ${fmtVersion(version)} to workspace`, 'ok');
}

async function copyThumbUrl() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  const url = await api(`/api/thumbnail-url?${qs({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version})}`);
  const copied = await copyText(url);
  status(copied ? 'Thumbnail URL copied' : `Clipboard blocked. Thumbnail URL: ${url}`, copied ? 'ok' : 'err');
}

function assetUri(asset, version) {
  const base = `ads://${asset.category}/${asset.asset_code}/${asset.department}/${asset.asset_code}.usd`;
  return version ? `${base}?v=${encodeURIComponent(version)}` : base;
}

function updateAssetUriField() {
  const asset = state.selected;
  if (!asset) return;
  $('assetUriInput').value = assetUri(asset, $('versionSelect').value);
}

async function copyAssetUri() {
  const asset = state.selected;
  if (!asset) return;
  updateAssetUriField();
  const uri = $('assetUriInput').value;
  const copied = await copyText(uri);
  status(copied ? 'ADS URI copied' : `Clipboard blocked. ADS URI: ${uri}`, copied ? 'ok' : 'err');
}

async function uploadThumbnail() {
  const asset = state.selected;
  const file = $('thumbnailInput').files[0];
  if (!asset || !file) return;
  const form = new FormData();
  form.set('profile', state.profile);
  form.set('category', asset.category);
  form.set('asset_code', asset.asset_code);
  form.set('department', asset.department);
  form.set('version', $('versionSelect').value);
  form.set('file', file);
  status('Uploading thumbnail…');
  await api('/api/thumbnails', {method: 'POST', body: form});
  await refreshSelection();
  status('Thumbnail uploaded', 'ok');
}

function assetKey(asset) {
  return `${asset.category}/${asset.asset_code}/${asset.department}`;
}

function showError(error) {
  status(error.message || String(error), 'err');
}

let searchTimer = null;
$('searchInput').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => loadAssets().catch(showError), 250);
});

$('authForm').addEventListener('submit', event => {
  event.preventDefault();
  state.token = $('tokenInput').value.trim();
  sessionStorage.setItem('adsToken', state.token);
  init().catch(showError);
});
$('profileSelect').addEventListener('change', () => {
  state.profile = $('profileSelect').value;
  state.category = '';
  state.department = '';
  loadAssets().catch(showError);
});
$('refreshButton').addEventListener('click', () => loadAssets().catch(showError));
$('logoutButton').addEventListener('click', () => { sessionStorage.removeItem('adsToken'); location.reload(); });
$('setCurrentButton').addEventListener('click', () => setCurrent().catch(showError));
$('resetCurrentButton').addEventListener('click', () => resetCurrent().catch(showError));
$('pullButton').addEventListener('click', () => pullToWorkspace().catch(showError));
$('versionSelect').addEventListener('change', () => {
  state.selectedVersion = $('versionSelect').value;
  updateAssetUriField();
  if (state.selected) {
    renderTakeLog({versions: state.versions, current_status: state.lastCurrentStatus || {}});
    loadManifest(state.selectedVersion).catch(showError);
  }
});
$('copyAssetUriButton').addEventListener('click', () => copyAssetUri().catch(showError));
$('copyThumbUrlButton').addEventListener('click', () => copyThumbUrl().catch(showError));
$('thumbnailInput').addEventListener('change', () => uploadThumbnail().catch(showError));

init().catch(showError);
"#;

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
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tempfile::TempDir;
    use tower::ServiceExt;

    #[test]
    fn version_id_parses_formats_and_increments() {
        let version = VersionId::from_str("v001").unwrap();
        assert_eq!(version, VersionId(1));
        assert_eq!(version.to_string(), "v001");
        assert_eq!(version.next().to_string(), "v002");
        assert_eq!(VersionId::from_str("v1000").unwrap().to_string(), "v1000");
        // Lenient parse: bare integers and zero-padded digits are canonical v8 forms.
        assert_eq!(VersionId::from_str("12").unwrap(), VersionId(12));
        assert_eq!(VersionId::from_str("001").unwrap(), VersionId(1));
        assert!(VersionId::from_str("v000").is_err());
        assert!(VersionId::from_str("0").is_err());
        assert!(VersionId::from_str("").is_err());
        assert!(VersionId::from_str("v").is_err());
        assert!(VersionId::from_str("v1a").is_err());
        // JSON canonical form is a number; both number and string deserialize.
        assert_eq!(serde_json::to_string(&VersionId(12)).unwrap(), "12");
        assert_eq!(
            serde_json::from_str::<VersionId>("12").unwrap(),
            VersionId(12)
        );
        assert_eq!(
            serde_json::from_str::<VersionId>("\"v012\"").unwrap(),
            VersionId(12)
        );
        // Fixed-width key encoding keeps lexicographic order numeric past v999.
        assert_eq!(VersionId(999).key_encode(), "0000000999");
        assert_eq!(VersionId(1000).key_encode(), "0000001000");
        assert!(VersionId(999).key_encode() < VersionId(1000).key_encode());
    }

    #[test]
    fn object_path_uses_sha256_prefix() {
        let path = object_path(
            Path::new("store"),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        assert_eq!(
            path,
            Path::new("store")
                .join("objects")
                .join("sha256")
                .join("ab")
                .join("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
    }

    #[test]
    fn asset_file_kind_splits_composing_formats_from_leaves() {
        // Composing formats carry relative sibling references and resolve
        // through the manifest view.
        assert_eq!(asset_file_kind("hero.usd"), AssetFileKind::Composing);
        assert_eq!(asset_file_kind("geo/body.USDA"), AssetFileKind::Composing);
        assert_eq!(asset_file_kind("mtl/look.mtlx"), AssetFileKind::Composing);
        // Everything else is a leaf and resolves lazily to the flat blob
        // cache — textures, volumes, caches, and unknown formats alike.
        assert_eq!(
            asset_file_kind("body_diffuse.1001.tx"),
            AssetFileKind::Leaf
        );
        assert_eq!(asset_file_kind("vol/smoke.vdb"), AssetFileKind::Leaf);
        assert_eq!(asset_file_kind("cache/custom.bin"), AssetFileKind::Leaf);
        assert_eq!(asset_file_kind("source/readme"), AssetFileKind::Leaf);
    }

    #[test]
    fn cache_object_path_uses_sha256_prefix_and_source_extension() {
        let entry = ManifestEntry {
            relative_path: "maps/body_diffuse.1001.TX".to_string(),
            sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            size: 10,
            mode: 0o666,
        };
        let path = cache_object_path(Path::new("workspace"), &entry);
        assert_eq!(
            path,
            Path::new("workspace")
                .join(".ads-cache")
                .join("sha256")
                .join("ab")
                .join("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789.tx")
        );
    }

    #[test]
    fn usd_asset_reference_scanner_extracts_at_paths() {
        let references = extract_usd_asset_references(
            r#"
            def "Root" (
                references = @ads://prop/crate/model/crate.usd?v=v001@
                payload = @../texture/v001/body.1001.tx@
            ) {}
            "#,
        );

        assert_eq!(
            references,
            vec![
                "ads://prop/crate/model/crate.usd?v=v001".to_string(),
                "../texture/v001/body.1001.tx".to_string(),
            ]
        );
    }

    #[test]
    fn publish_reference_validation_applies_v8_policy() {
        let mut report = PublishValidateReport {
            target: "version 1".to_string(),
            files_scanned: 1,
            references_checked: 0,
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        let entry_paths: BTreeSet<String> = ["asset.usda", "geo/body.usd", "maps/d.tx"]
            .into_iter()
            .map(str::to_string)
            .collect();

        // ads:// and manifest-internal relative references are accepted.
        validate_publish_reference(
            &mut report,
            &entry_paths,
            "asset.usda",
            "ads://prop/crate/model/crate.usd?v=2",
        );
        validate_publish_reference(&mut report, &entry_paths, "asset.usda", "geo/body.usd");
        validate_publish_reference(&mut report, &entry_paths, "geo/body.usd", "../maps/d.tx");
        assert!(report.errors.is_empty());

        // Absolute paths, file URIs, missing siblings, and escapes are errors.
        validate_publish_reference(
            &mut report,
            &entry_paths,
            "asset.usda",
            r"D:\workspace\asset.usd",
        );
        validate_publish_reference(&mut report, &entry_paths, "asset.usda", "file:///tmp/a.usd");
        validate_publish_reference(&mut report, &entry_paths, "asset.usda", "geo/missing.usd");
        validate_publish_reference(&mut report, &entry_paths, "asset.usda", "../outside.usd");
        validate_publish_reference(
            &mut report,
            &entry_paths,
            "asset.usda",
            "https://example.com/asset.usd",
        );

        assert_eq!(report.errors.len(), 4);
        assert!(report.errors[0].contains("absolute path"));
        assert!(report.errors[1].contains("file URI"));
        assert!(report.errors[2].contains("missing from the version"));
        assert!(report.errors[3].contains("escapes the version root"));
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn remote_client_url_encodes_nested_category() {
        let client = RemoteClient::new("http://server:8787/", "secret").unwrap();
        let url = client.url(
            "/api/version",
            &[
                ("profile", "main".to_string()),
                ("category", "assets/characters/main".to_string()),
                ("asset_code", "hero".to_string()),
            ],
        );

        assert_eq!(
            url,
            "http://server:8787/api/version?profile=main&category=assets%2Fcharacters%2Fmain&asset_code=hero"
        );
    }

    #[test]
    fn adsignore_matches_root_and_nested_files() {
        let rules = IgnoreRules {
            rules: vec![
                IgnoreRule::new("*.tmp").unwrap(),
                IgnoreRule::new("cache/").unwrap(),
                IgnoreRule::new("nested/generated.dat").unwrap(),
            ],
        };

        assert!(rules.is_ignored(Path::new("file.tmp"), false));
        assert!(rules.is_ignored(Path::new("nested/file.tmp"), false));
        assert!(rules.is_ignored(Path::new("cache"), true));
        assert!(!rules.is_ignored(Path::new("cache/file.txt"), false));
        assert!(rules.is_ignored(Path::new("nested/generated.dat"), false));
        assert!(!rules.is_ignored(Path::new("nested/kept.dat"), false));
        assert!(is_default_ignored(Path::new(".ads-cache"), true));
        assert!(is_default_ignored(
            Path::new(".ads-cache/sha256/ab/object.tx"),
            false
        ));
    }

    #[test]
    fn manifest_hash_is_stable_after_sorting() {
        let mut first = Manifest {
            entries: vec![
                ManifestEntry {
                    relative_path: "b.txt".to_string(),
                    sha256: "b".repeat(64),
                    size: 2,
                    mode: 0o666,
                },
                ManifestEntry {
                    relative_path: "a.txt".to_string(),
                    sha256: "a".repeat(64),
                    size: 1,
                    mode: 0o666,
                },
            ],
        };
        let mut second = Manifest {
            entries: first.entries.iter().cloned().rev().collect(),
        };
        first
            .entries
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        second
            .entries
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

        assert_eq!(
            first.canonical_hash().unwrap(),
            second.canonical_hash().unwrap()
        );
    }

    #[test]
    fn add_reuses_identical_manifest() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("workspace");

        let store = Store::init(&store_path).unwrap();
        let key = AssetKey::new("char".to_string(), "hero".to_string()).unwrap();
        let department_key = DepartmentKey::new(key, "model".to_string()).unwrap();
        fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
        fs::write(
            version_folder(&workspace, &department_key, VersionId(1)).join("asset.usd"),
            "usd content",
        )
        .unwrap();
        let first = store
            .add_version_folder(&workspace, &department_key, VersionId(1))
            .unwrap();
        let second = store
            .add_version_folder(&workspace, &department_key, VersionId(1))
            .unwrap();

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.version, second.version);
    }

    #[tokio::test]
    async fn web_api_requires_bearer_token_and_serves_static_ui() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        Store::init(&store_path).unwrap();
        let app = web_app(test_web_state(&store_path, &workspace));

        let public = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(public.status(), StatusCode::OK);
        assert!(response_text(public).await.contains("ADS Asset Browser"));

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let wrong = app
            .clone()
            .oneshot(api_request("GET", "/api/profiles", "wrong", Body::empty()))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let ok = app
            .clone()
            .oneshot(api_request("GET", "/api/profiles", "secret", Body::empty()))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let body = response_json(ok).await;
        assert_eq!(body["profiles"][0]["name"], "main");

        let missing_profile = app
            .oneshot(api_request(
                "GET",
                "/api/assets?profile=missing",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(missing_profile.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn web_api_lists_assets_updates_current_and_pulls() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        let store = Store::init(&store_path).unwrap();
        store
            .set_remote_base_url("https://assets.example.com/objects/sha256")
            .unwrap();
        let department_key = DepartmentKey::new(
            AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
            "model".to_string(),
        )
        .unwrap();
        fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
        fs::write(
            version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd"),
            "v1",
        )
        .unwrap();
        store
            .add_version_folder(&workspace, &department_key, VersionId(1))
            .unwrap();
        fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(2))).unwrap();
        fs::write(
            version_folder(&workspace, &department_key, VersionId(2)).join("crate.usd"),
            "v2",
        )
        .unwrap();
        store
            .add_version_folder(&workspace, &department_key, VersionId(2))
            .unwrap();
        let thumb = temp.path().join("thumb.png");
        fs::write(&thumb, test_png_1x1()).unwrap();
        store
            .set_thumbnail(&department_key, VersionId(2), &thumb)
            .unwrap();
        let nested_department_key = DepartmentKey::new(
            AssetKey::new("prop/vehicle".to_string(), "truck".to_string()).unwrap(),
            "model".to_string(),
        )
        .unwrap();
        fs::create_dir_all(version_folder(
            &workspace,
            &nested_department_key,
            VersionId(1),
        ))
        .unwrap();
        fs::write(
            version_folder(&workspace, &nested_department_key, VersionId(1)).join("truck.usd"),
            "truck-v1",
        )
        .unwrap();
        store
            .add_version_folder(&workspace, &nested_department_key, VersionId(1))
            .unwrap();
        drop(store);

        let app = web_app(test_web_state(&store_path, &workspace));
        let assets = app
            .clone()
            .oneshot(api_request(
                "GET",
                "/api/assets?profile=main",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(assets.status(), StatusCode::OK);
        let assets = response_json(assets).await;
        assert_eq!(assets["assets"][0]["category"], "prop");
        assert_eq!(assets["assets"][0]["asset_code"], "crate");
        assert_eq!(assets["assets"][0]["department"], "model");
        assert_eq!(assets["assets"][0]["current"], 2);
        assert!(
            assets["assets"][0]["thumbnail_url"]
                .as_str()
                .unwrap()
                .starts_with("https://assets.example.com/objects/sha256/")
        );

        let prefixed_assets = app
            .clone()
            .oneshot(api_request(
                "GET",
                "/api/assets?profile=main&category=prop/veh",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(prefixed_assets.status(), StatusCode::OK);
        let prefixed_assets = response_json(prefixed_assets).await;
        assert_eq!(prefixed_assets["assets"].as_array().unwrap().len(), 1);
        assert_eq!(prefixed_assets["assets"][0]["category"], "prop/vehicle");
        assert_eq!(prefixed_assets["assets"][0]["asset_code"], "truck");

        let version_info = app
            .clone()
            .oneshot(api_request(
                "GET",
                "/api/version?profile=main&category=prop&asset_code=crate&department=model&version=v002",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(version_info.status(), StatusCode::OK);
        let version_info = response_json(version_info).await;
        assert_eq!(version_info["version"]["version"], 2);
        assert_eq!(
            version_info["manifest"]["entries"][0]["relative_path"],
            "crate.usd"
        );

        let v2_hash = sha256_bytes(b"v2");
        let object = app
            .clone()
            .oneshot(api_request(
                "GET",
                &format!("/api/object?profile=main&sha256={v2_hash}"),
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(object.status(), StatusCode::OK);
        assert_eq!(
            object.headers().get("x-ads-sha256").unwrap(),
            v2_hash.as_str()
        );
        assert_eq!(response_bytes(object).await, b"v2");

        let set_current = app
            .clone()
            .oneshot(api_request(
                "PUT",
                "/api/current",
                "secret",
                Body::from(
                    r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model","version":"v001"}"#,
                ),
            ))
            .await
            .unwrap();
        assert_eq!(set_current.status(), StatusCode::OK);
        let set_current = response_json(set_current).await;
        assert_eq!(set_current["current"], 1);
        assert_eq!(set_current["explicit"], true);

        // Schema v8: pull seeds the department work folder (no v### name),
        // the same root the WIP staging processor redirects from.
        fs::remove_dir_all(department_folder(&workspace, &department_key)).unwrap();
        let pull = app
            .oneshot(api_request(
                "POST",
                "/api/pull",
                "secret",
                Body::from(
                    r#"{"profile":"main","category":"prop","asset_code":"crate","department":"model","version":"v001"}"#,
                ),
            ))
            .await
            .unwrap();
        assert_eq!(pull.status(), StatusCode::OK);
        assert_eq!(
            fs::read_to_string(department_folder(&workspace, &department_key).join("crate.usd"))
                .unwrap(),
            "v1"
        );
    }

    #[tokio::test]
    async fn web_api_uploads_thumbnail_and_returns_url() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        let store = Store::init(&store_path).unwrap();
        store
            .set_remote_base_url("https://assets.example.com/objects/sha256")
            .unwrap();
        let department_key = DepartmentKey::new(
            AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
            "model".to_string(),
        )
        .unwrap();
        fs::create_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
        fs::write(
            version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd"),
            "v1",
        )
        .unwrap();
        store
            .add_version_folder(&workspace, &department_key, VersionId(1))
            .unwrap();
        drop(store);

        let app = web_app(test_web_state(&store_path, &workspace));
        let boundary = "ADSBOUNDARY";
        let body = multipart_thumbnail_body(boundary);
        let upload = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/thumbnails")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::OK);
        let upload = response_json(upload).await;
        assert_eq!(upload["mime_type"], "image/png");
        assert_eq!(upload["width"], 1);

        let thumb_hash = sha256_bytes(test_png_1x1());
        let url = app
            .oneshot(api_request(
                "GET",
                "/api/thumbnail-url?profile=main&category=prop&asset_code=crate&department=model&version=v001",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(url.status(), StatusCode::OK);
        let url = response_json(url).await;
        assert_eq!(
            url.as_str().unwrap(),
            format!(
                "https://assets.example.com/objects/sha256/{}/{}",
                &thumb_hash[0..2],
                thumb_hash
            )
        );
    }

    #[tokio::test]
    async fn web_api_accepts_object_and_version_import() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        Store::init(&store_path).unwrap();
        let app = web_app(test_web_state(&store_path, &workspace));

        let object_bytes = b"remote-v1";
        let object_hash = sha256_bytes(object_bytes);
        let status = app
            .clone()
            .oneshot(api_request(
                "GET",
                &format!(
                    "/api/object/status?profile=main&sha256={object_hash}&size={}",
                    object_bytes.len()
                ),
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        assert_eq!(response_json(status).await["exists"], false);

        let upload = app
            .clone()
            .oneshot(api_request(
                "PUT",
                &format!("/api/object?profile=main&sha256={object_hash}"),
                "secret",
                Body::from(object_bytes.as_slice()),
            ))
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::OK);
        let upload = response_json(upload).await;
        assert_eq!(upload["sha256"], object_hash);
        assert_eq!(upload["reused"], false);

        let department_key = DepartmentKey::new(
            AssetKey::new("prop".to_string(), "crate".to_string()).unwrap(),
            "model".to_string(),
        )
        .unwrap();
        let manifest = Manifest {
            entries: vec![ManifestEntry {
                relative_path: "crate.usd".to_string(),
                sha256: object_hash.clone(),
                size: object_bytes.len() as u64,
                mode: 0o666,
            }],
        };
        let version_info = VersionInfo {
            version: VersionRecord {
                department_key: department_key.clone(),
                version: VersionId(1),
                manifest_hash: manifest.canonical_hash().unwrap(),
                created_at: "2026-05-27T00:00:00Z".to_string(),
                source_path: "prop/crate/model/v001".to_string(),
                file_count: 1,
                total_bytes: object_bytes.len() as u64,
                promoted_from: None,
            },
            manifest,
        };
        let import = app
            .clone()
            .oneshot(api_request(
                "PUT",
                "/api/version",
                "secret",
                Body::from(
                    serde_json::to_vec(&VersionImportRequest {
                        profile: "main".to_string(),
                        version_info,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(import.status(), StatusCode::OK);
        assert_eq!(response_json(import).await["version"], 1);

        let fetched = app
            .clone()
            .oneshot(api_request(
                "GET",
                "/api/version?profile=main&category=prop&asset_code=crate&department=model&version=v001",
                "secret",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(fetched.status(), StatusCode::OK);
        let fetched = response_json(fetched).await;
        assert_eq!(fetched["manifest"]["entries"][0]["sha256"], object_hash);

        let thumbnail = ThumbnailRecord {
            department_key,
            version: VersionId(1),
            sha256: object_hash.clone(),
            size: object_bytes.len() as u64,
            mime_type: "image/png".to_string(),
            width: Some(256),
            height: Some(256),
            created_at: "2026-05-27T00:00:00Z".to_string(),
            source_path: "thumbnail.png".to_string(),
        };
        let import_thumbnail = app
            .oneshot(api_request(
                "PUT",
                "/api/thumbnail",
                "secret",
                Body::from(
                    serde_json::to_vec(&ThumbnailImportRequest {
                        profile: "main".to_string(),
                        thumbnail,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(import_thumbnail.status(), StatusCode::OK);
        let import_thumbnail = response_json(import_thumbnail).await;
        assert_eq!(import_thumbnail["sha256"], object_hash);
        assert_eq!(import_thumbnail["width"], 256);
    }

    fn test_web_state(store_path: &Path, workspace: &Path) -> Arc<WebState> {
        let profile = ServeProfile::new(
            "main".to_string(),
            store_path.to_path_buf(),
            workspace.to_path_buf(),
        )
        .unwrap();
        Arc::new(
            WebState::try_new(ServeConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                auth_token: "secret".to_string(),
                profiles: BTreeMap::from([(profile.name.clone(), profile)]),
                max_upload_bytes: 10 * 1024 * 1024,
                max_object_upload_bytes: 1024 * 1024 * 1024,
            })
            .unwrap(),
        )
    }

    fn api_request(method: &str, uri: &str, token: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    async fn response_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn response_bytes(response: Response) -> Vec<u8> {
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn multipart_thumbnail_body(boundary: &str) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in [
            ("profile", "main"),
            ("category", "prop"),
            ("asset_code", "crate"),
            ("department", "model"),
            ("version", "v001"),
        ] {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"thumb.png\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.extend_from_slice(test_png_1x1());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    fn test_png_1x1() -> &'static [u8] {
        &[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xae,
            0x42, 0x60, 0x82,
        ]
    }
}
