use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{DefaultBodyLimit, Multipart, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobMatcher};
use rocksdb::{DB, IteratorMode, Options, WriteBatch};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use walkdir::{DirEntry, WalkDir};

const SCHEMA_VERSION: &str = "7";
const DB_DIR: &str = "db";
const OBJECTS_DIR: &str = "objects";
const SHA256_DIR: &str = "sha256";

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
    /// Register a standard workspace version folder.
    Add {
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
        /// Version folder to register, for example v001.
        #[arg(long)]
        version: VersionId,
    },
    /// Create the next editable version folder.
    NewVersion {
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
    /// Pull the current workspace version folder from the store.
    Pull(WorkspacePullArgs),
    /// Restore a specific version into its standard workspace version folder.
    Restore(WorkspaceRestoreArgs),
    /// Deprecated alias for `pull` / `restore`.
    #[command(hide = true)]
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
        /// Version to restore. Defaults to the current version.
        #[arg(long)]
        version: Option<VersionId>,
        /// Restore the latest version instead of the current version.
        #[arg(long)]
        latest: bool,
        /// Replace a different existing version folder.
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
        /// Asset path such as ads://hero/model/hero.usd or ads://char/hero/model/hero.usd?v=v002.
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
    },
    /// Verify metadata and object content.
    Verify {
        /// Store root path.
        #[arg(long)]
        store: PathBuf,
    },
}

#[derive(Args, Debug)]
struct WorkspacePullArgs {
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
    /// Pull the latest version instead of the current version.
    #[arg(long)]
    latest: bool,
    /// Replace a different existing version folder.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct WorkspaceRestoreArgs {
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
    /// Version to restore, for example v001.
    #[arg(long)]
    version: VersionId,
    /// Replace a different existing version folder.
    #[arg(long)]
    force: bool,
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
        } => {
            let asset_key = AssetKey::new(category, asset_code)?;
            let department_key = DepartmentKey::new(asset_key, department)?;
            let workspace = workspace_root(workspace)?;
            let store = Store::open(&store)?;
            let outcome = store.add_version_folder(&workspace, &department_key, version)?;
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
        Commands::NewVersion {
            store,
            workspace,
            category,
            asset_code,
            department,
        } => {
            let asset_key = AssetKey::new(category, asset_code)?;
            let department_key = DepartmentKey::new(asset_key, department)?;
            let workspace = workspace_root(workspace)?;
            let store = Store::open(&store)?;
            let outcome = store.new_version_folder(&workspace, &department_key)?;
            if let Some(from_version) = outcome.from_version {
                println!(
                    "created {} {}/{}/{} from {} at {}",
                    outcome.version,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    from_version,
                    outcome.path.display()
                );
            } else {
                println!(
                    "created {} {}/{}/{} at {}",
                    outcome.version,
                    department_key.asset_key.category,
                    department_key.asset_key.asset_code,
                    department_key.department,
                    outcome.path.display()
                );
            }
        }
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
                let store = Store::open(&store)?;
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
                let store = Store::open(&store)?;
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
                let store = Store::open(&store)?;
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
                let store = Store::open(&store)?;
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
                let store = Store::open(&store)?;
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
                let store = Store::open(&store)?;
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
                let store = Store::open(&store)?;
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

            let store = Store::open(&store)?;
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
            let store = Store::open(&store)?;
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
            let store = Store::open(&store)?;
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
        Commands::Pull(args) => {
            let asset_key = AssetKey::new(args.category, args.asset_code)?;
            let department_key = DepartmentKey::new(asset_key, args.department)?;
            let selector = if args.latest {
                VersionSelector::Latest
            } else {
                VersionSelector::Current
            };
            restore_standard_workspace_version(
                &args.store,
                args.workspace,
                department_key,
                selector,
                args.force,
                WorkspaceRestoreWords::new("pulled", "already pulled"),
            )?;
        }
        Commands::Restore(args) => {
            let asset_key = AssetKey::new(args.category, args.asset_code)?;
            let department_key = DepartmentKey::new(asset_key, args.department)?;
            restore_standard_workspace_version(
                &args.store,
                args.workspace,
                department_key,
                VersionSelector::Version(args.version),
                args.force,
                WorkspaceRestoreWords::new("restored", "already restored"),
            )?;
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
            let store = Store::open(&store)?;
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
            let store = Store::open(&store)?;
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
        } => {
            let config = ServeConfig::from_args(
                bind,
                auth_token,
                profiles,
                store,
                workspace,
                max_upload_mb,
            )?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build Tokio runtime")?;
            runtime.block_on(serve_web(config))?;
        }
        Commands::Verify { store } => {
            let store = Store::open(&store)?;
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

fn restore_standard_workspace_version(
    store: &Path,
    workspace: Option<PathBuf>,
    department_key: DepartmentKey,
    selector: VersionSelector,
    force: bool,
    words: WorkspaceRestoreWords,
) -> Result<()> {
    let workspace = workspace_root(workspace)?;
    let store = Store::open(store)?;
    let outcome = store.materialize(&workspace, &department_key, selector, force)?;
    print_workspace_restore_outcome(&department_key, outcome, words);
    Ok(())
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
}

impl fmt::Display for VersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{:03}", self.0)
    }
}

impl FromStr for VersionId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let Some(digits) = value.strip_prefix('v') else {
            bail!("version must start with 'v': {value}");
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            bail!("version must be formatted like v001: {value}");
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
        serializer.serialize_str(&self.to_string())
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
                formatter.write_str("a version string like v001")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                VersionId::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(VersionVisitor)
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
        VersionId::from_str(value).ok().map(Self::Version)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolveSource {
    Local,
    Remote,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolveOutcome {
    pub location: String,
    pub source: ResolveSource,
    pub version: VersionId,
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<ManifestEntry>,
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

#[derive(Clone, Debug, Serialize)]
pub struct CreateAssetOutcome {
    pub asset: AssetRecord,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct NewVersionOutcome {
    pub version: VersionId,
    pub from_version: Option<VersionId>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct MaterializeOutcome {
    pub version: VersionId,
    pub path: PathBuf,
    pub unchanged: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AssetInfo {
    pub asset: AssetRecord,
    pub current_versions: BTreeMap<String, VersionId>,
    pub versions: Vec<VersionRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct VersionInfo {
    pub version: VersionRecord,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize)]
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
}

#[derive(Clone)]
struct WebProfile {
    name: String,
    store: PathBuf,
    workspace: PathBuf,
    mutation_lock: Arc<Mutex<()>>,
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

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
struct AssetsResponse {
    assets: Vec<AssetCardDto>,
}

#[derive(Clone, Debug, Serialize)]
struct AssetDetailResponse {
    info: AssetInfo,
    current_status: Vec<CurrentStatus>,
    thumbnails: Vec<ThumbnailRecord>,
}

#[derive(Clone, Debug, Serialize)]
struct VersionsResponse {
    versions: Vec<VersionRecord>,
    current_status: CurrentStatus,
    thumbnails: Vec<ThumbnailRecord>,
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
        let schema = db
            .get(key_meta("schema_version"))?
            .ok_or_else(|| anyhow!("store metadata is missing schema_version"))?;
        if schema.as_slice() != SCHEMA_VERSION.as_bytes() {
            bail!(
                "unsupported store schema version {}; expected {SCHEMA_VERSION}",
                String::from_utf8_lossy(&schema)
            );
        }
        Ok(Self {
            root: path.to_path_buf(),
            db,
        })
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

    pub fn new_version_folder(
        &self,
        workspace: &Path,
        department_key: &DepartmentKey,
    ) -> Result<NewVersionOutcome> {
        let latest = self.latest_version(department_key)?;
        let version = latest.map_or(VersionId(1), VersionId::next);
        let path = version_folder(workspace, department_key, version);
        self.prepare_checkout_dest(&path, false)?;

        if let Some(from_version) = latest {
            let record = self.get_version(department_key, from_version)?;
            let manifest = self.get_manifest(&record.manifest_hash)?;
            self.restore_manifest_to_dest(&manifest, &path)?;
        }

        Ok(NewVersionOutcome {
            version,
            from_version: latest,
            path,
        })
    }

    pub fn add_version_folder(
        &self,
        workspace: &Path,
        department_key: &DepartmentKey,
        version: VersionId,
    ) -> Result<AddOutcome> {
        let source = version_folder(workspace, department_key, version);
        let source = source.canonicalize().with_context(|| {
            format!(
                "version folder does not exist: {}; run `ads new-version` first",
                source.display()
            )
        })?;
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
            source_path: version_workspace_relative_path(department_key, version),
            file_count: manifest.entries.len() as u64,
            total_bytes: manifest.total_bytes(),
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
        batch.put(key_latest(department_key), version.to_string().as_bytes());
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
        let path = version_folder(workspace, department_key, version);

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
                    "version folder exists and is not empty: {}; pass --force to replace it",
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
        let local_path = safe_join(
            &version_folder(workspace, &asset_path.department_key, version),
            &asset_path.relative_path,
        )?;

        match mode {
            ResolveMode::Local => {
                if !local_path.exists() {
                    bail!("local asset path does not exist: {}", local_path.display());
                }
                Ok(ResolveOutcome {
                    location: local_path.display().to_string(),
                    source: ResolveSource::Local,
                    version,
                    sha256: entry.sha256.clone(),
                })
            }
            ResolveMode::Remote => Ok(ResolveOutcome {
                location: self.resolve_remote_url(entry, remote_base_url_override)?,
                source: ResolveSource::Remote,
                version,
                sha256: entry.sha256.clone(),
            }),
            ResolveMode::Auto => {
                if local_path.exists() {
                    Ok(ResolveOutcome {
                        location: local_path.display().to_string(),
                        source: ResolveSource::Local,
                        version,
                        sha256: entry.sha256.clone(),
                    })
                } else {
                    Ok(ResolveOutcome {
                        location: self.resolve_remote_url(entry, remote_base_url_override)?,
                        source: ResolveSource::Remote,
                        version,
                        sha256: entry.sha256.clone(),
                    })
                }
            }
        }
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
            .put(key_current(department_key), version.to_string().as_bytes())?;
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
                    if self.selected_version(&department_key, version)?.is_some() {
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
                if self.selected_version(&department_key, version)?.is_some() {
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
            VersionSelector::Version(version) => Ok(self
                .try_get_version(department_key, version)?
                .map(|_| version)),
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
        self.db
            .get(key_thumbnail(department_key, version))?
            .map(|value| {
                serde_json::from_slice(&value).context("failed to decode thumbnail record")
            })
            .transpose()?
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

    fn get_manifest(&self, manifest_hash: &str) -> Result<Manifest> {
        let value = self
            .db
            .get(key_manifest(manifest_hash))?
            .ok_or_else(|| anyhow!("manifest not found: {manifest_hash}"))?;
        serde_json::from_slice(&value).context("failed to decode manifest")
    }
}

impl ServeConfig {
    fn from_args(
        bind: SocketAddr,
        auth_token: Option<String>,
        profiles: Vec<String>,
        store: Option<PathBuf>,
        workspace: Option<PathBuf>,
        max_upload_mb: u64,
    ) -> Result<Self> {
        let auth_token = auth_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow!("--auth-token or ADS_WEB_TOKEN is required for `ads serve`"))?;
        let max_upload_bytes = max_upload_mb
            .checked_mul(1024)
            .and_then(|value| value.checked_mul(1024))
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| anyhow!("--max-upload-mb is too large"))?;
        if max_upload_bytes == 0 {
            bail!("--max-upload-mb must be greater than zero");
        }

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

impl From<ServeConfig> for WebState {
    fn from(config: ServeConfig) -> Self {
        let profiles = config
            .profiles
            .into_iter()
            .map(|(name, profile)| {
                (
                    name,
                    WebProfile {
                        name: profile.name,
                        store: profile.store,
                        workspace: profile.workspace,
                        mutation_lock: Arc::new(Mutex::new(())),
                    },
                )
            })
            .collect();
        Self {
            auth_token: config.auth_token,
            profiles: Arc::new(profiles),
            max_upload_bytes: config.max_upload_bytes,
        }
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
    let state = Arc::new(WebState::from(config));
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
    let api = Router::new()
        .route("/profiles", get(api_profiles))
        .route("/assets", get(api_assets))
        .route("/asset", get(api_asset))
        .route("/versions", get(api_versions))
        .route("/current/status", get(api_current_status))
        .route("/current", put(api_update_current))
        .route("/pull", post(api_pull))
        .route("/restore", post(api_restore))
        .route("/materialize", post(api_materialize))
        .route("/thumbnails", post(api_upload_thumbnail))
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
        .layer(DefaultBodyLimit::max(max_upload_bytes))
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
    if token == Some(state.auth_token.as_str()) {
        Ok(next.run(request).await)
    } else {
        Err(ApiError::unauthorized("missing or invalid bearer token"))
    }
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
        let store = Store::open(&profile.store)?;
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
        let store = Store::open(&profile.store)?;
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
        let store = Store::open(&profile.store)?;
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
        let store = Store::open(&profile.store)?;
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
        let store = Store::open(&profile.store)?;
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
        let store = Store::open(&profile.store)?;
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
            let store = Store::open(&profile.store)?;
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

async fn api_thumbnail_url(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ThumbnailUrlQuery>,
) -> std::result::Result<Json<String>, ApiError> {
    let profile = profile_for(&state, &query.profile)?;
    run_store_read(move || {
        let store = Store::open(&profile.store)?;
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
        let store = Store::open(&profile.store)?;
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
        query.category.as_deref(),
        query.asset_code.as_deref(),
        query.department.as_deref(),
    )?;
    let statuses = store.current_status(
        query.category.as_deref(),
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
<html lang="ja">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ADS Asset Browser</title>
  <link rel="stylesheet" href="/style.css">
</head>
<body>
  <div id="auth" class="auth">
    <form id="authForm" class="auth-panel">
      <h1>ADS Asset Browser</h1>
      <input id="tokenInput" type="password" autocomplete="current-password" placeholder="Bearer token">
      <button type="submit">Connect</button>
    </form>
  </div>

  <main class="shell">
    <aside class="sidebar">
      <div class="brand">ADS</div>
      <label>Profile<select id="profileSelect"></select></label>
      <label>Search<input id="searchInput" type="search" placeholder="asset, category, department"></label>
      <label>Category<select id="categoryFilter"><option value="">All categories</option></select></label>
      <label>Department<select id="departmentFilter"><option value="">All departments</option></select></label>
      <button id="refreshButton" type="button">Refresh</button>
      <button id="logoutButton" type="button" class="ghost">Lock</button>
      <div id="status" class="status"></div>
    </aside>

    <section class="browser">
      <header class="toolbar">
        <div>
          <h2>Assets</h2>
          <span id="assetCount">0 assets</span>
        </div>
      </header>
      <div id="assetGrid" class="asset-grid"></div>
    </section>

    <aside class="detail">
      <div id="detailEmpty" class="empty">Select an asset</div>
      <div id="detailPanel" class="detail-panel hidden">
        <div class="detail-head">
          <div>
            <h2 id="detailTitle"></h2>
            <p id="detailSubtitle"></p>
          </div>
          <span id="detailDepartment" class="pill"></span>
        </div>
        <div class="preview" id="detailPreview"></div>
        <div class="field-row">
          <label>Version<select id="versionSelect"></select></label>
          <label class="check"><input id="forcePull" type="checkbox"> Force</label>
        </div>
        <div class="button-row">
          <button id="setCurrentButton" type="button">Set Current</button>
          <button id="resetCurrentButton" type="button">Reset</button>
          <button id="pullButton" type="button">Pull to Workspace</button>
        </div>
        <div class="button-row">
          <button id="copyThumbUrlButton" type="button">Copy Thumbnail URL</button>
          <label class="upload">
            Upload Thumbnail
            <input id="thumbnailInput" type="file" accept="image/png,image/jpeg,image/webp">
          </label>
        </div>
        <section>
          <h3>Versions</h3>
          <div id="versionList" class="version-list"></div>
        </section>
      </div>
    </aside>
  </main>
  <script src="/app.js"></script>
</body>
</html>
"#;

const STYLE_CSS: &str = r#":root {
  color-scheme: dark;
  --bg: #171717;
  --panel: #202020;
  --panel-2: #262626;
  --line: #363636;
  --text: #ece7dc;
  --muted: #a59f93;
  --accent: #d6ff67;
  --accent-2: #58d0a7;
  --danger: #ff7a68;
}

* { box-sizing: border-box; }
body {
  margin: 0;
  min-height: 100vh;
  background: var(--bg);
  color: var(--text);
  font-family: "Segoe UI", system-ui, sans-serif;
  letter-spacing: 0;
}
button, input, select { font: inherit; }
button {
  min-height: 34px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: var(--accent);
  color: #111;
  padding: 0 12px;
  cursor: pointer;
}
button.ghost {
  background: transparent;
  color: var(--text);
}
input, select {
  width: 100%;
  min-height: 34px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: #121212;
  color: var(--text);
  padding: 0 10px;
}
label {
  display: grid;
  gap: 6px;
  color: var(--muted);
  font-size: 12px;
}
.shell {
  display: grid;
  grid-template-columns: 260px minmax(360px, 1fr) 380px;
  min-height: 100vh;
}
.sidebar, .detail {
  background: var(--panel);
  border-color: var(--line);
  padding: 18px;
}
.sidebar {
  display: grid;
  align-content: start;
  gap: 14px;
  border-right: 1px solid var(--line);
}
.detail { border-left: 1px solid var(--line); }
.brand {
  font-size: 28px;
  font-weight: 700;
  color: var(--accent);
}
.browser {
  display: grid;
  grid-template-rows: auto 1fr;
  min-width: 0;
}
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 18px 22px;
  border-bottom: 1px solid var(--line);
}
.toolbar h2, .detail h2, .detail h3 { margin: 0; }
.toolbar span, .status, .detail p { color: var(--muted); }
.asset-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(190px, 1fr));
  gap: 14px;
  padding: 18px;
  align-content: start;
  overflow: auto;
}
.asset-card {
  display: grid;
  grid-template-rows: 130px auto;
  border: 1px solid var(--line);
  border-radius: 8px;
  overflow: hidden;
  background: var(--panel-2);
  cursor: pointer;
}
.asset-card.active { outline: 2px solid var(--accent); }
.thumb {
  display: grid;
  place-items: center;
  background: #101010;
  color: var(--muted);
  min-height: 130px;
  overflow: hidden;
}
.thumb img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.asset-meta {
  display: grid;
  gap: 6px;
  padding: 10px;
}
.asset-name {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  font-weight: 650;
}
.asset-sub {
  color: var(--muted);
  font-size: 12px;
  overflow-wrap: anywhere;
}
.pill {
  display: inline-flex;
  align-items: center;
  width: fit-content;
  min-height: 24px;
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 0 8px;
  color: var(--accent-2);
  font-size: 12px;
}
.detail-panel { display: grid; gap: 16px; }
.hidden { display: none !important; }
.empty {
  display: grid;
  place-items: center;
  min-height: 240px;
  color: var(--muted);
}
.detail-head {
  display: flex;
  align-items: start;
  justify-content: space-between;
  gap: 12px;
}
.preview {
  display: grid;
  place-items: center;
  min-height: 210px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: #111;
  overflow: hidden;
}
.preview img {
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.field-row, .button-row {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px;
}
.button-row { grid-template-columns: repeat(3, 1fr); }
.check {
  display: flex;
  align-items: end;
  gap: 8px;
}
.check input { width: auto; }
.upload {
  display: grid;
  place-items: center;
  min-height: 34px;
  border: 1px solid var(--line);
  border-radius: 6px;
  background: #151515;
  color: var(--text);
  cursor: pointer;
}
.upload input { display: none; }
.version-list {
  display: grid;
  gap: 8px;
}
.version-row {
  display: grid;
  grid-template-columns: 56px 1fr auto;
  gap: 10px;
  align-items: center;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 8px;
  background: #181818;
}
.auth {
  position: fixed;
  inset: 0;
  z-index: 5;
  display: grid;
  place-items: center;
  background: rgba(10, 10, 10, .92);
}
.auth.hidden { display: none; }
.auth-panel {
  display: grid;
  gap: 14px;
  width: min(360px, calc(100vw - 32px));
  padding: 24px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--panel);
}
@media (max-width: 980px) {
  .shell { grid-template-columns: 220px 1fr; }
  .detail { grid-column: 1 / -1; border-left: 0; border-top: 1px solid var(--line); }
}
"#;

const APP_JS: &str = r#"const state = {
  token: sessionStorage.getItem('adsToken') || '',
  profile: '',
  assets: [],
  selected: null,
  versions: [],
  selectedVersion: ''
};

const $ = (id) => document.getElementById(id);
const status = (text) => { $('status').textContent = text || ''; };
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));

async function api(path, options = {}) {
  const headers = options.headers ? new Headers(options.headers) : new Headers();
  headers.set('Authorization', `Bearer ${state.token}`);
  if (options.body && !(options.body instanceof FormData)) headers.set('Content-Type', 'application/json');
  const res = await fetch(path, {...options, headers});
  if (res.status === 401) {
    sessionStorage.removeItem('adsToken');
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
    return;
  }
  $('auth').classList.add('hidden');
  const data = await api('/api/profiles');
  const select = $('profileSelect');
  select.innerHTML = data.profiles.map(p => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join('');
  state.profile = select.value || data.profiles[0]?.name || '';
  await loadAssets();
}

async function loadAssets() {
  if (!state.profile) return;
  status('Loading');
  const params = {
    profile: state.profile,
    q: $('searchInput').value,
    category: $('categoryFilter').value,
    department: $('departmentFilter').value
  };
  const data = await api(`/api/assets?${qs(params)}`);
  state.assets = data.assets;
  renderFilters(data.assets);
  renderAssets();
  status('');
}

function renderFilters(assets) {
  const currentCategory = $('categoryFilter').value;
  const currentDepartment = $('departmentFilter').value;
  const categories = [...new Set(assets.map(a => a.category))].sort();
  const departments = [...new Set(assets.map(a => a.department))].sort();
  $('categoryFilter').innerHTML = '<option value="">All categories</option>' + categories.map(v => `<option value="${esc(v)}">${esc(v)}</option>`).join('');
  $('departmentFilter').innerHTML = '<option value="">All departments</option>' + departments.map(v => `<option value="${esc(v)}">${esc(v)}</option>`).join('');
  $('categoryFilter').value = categories.includes(currentCategory) ? currentCategory : '';
  $('departmentFilter').value = departments.includes(currentDepartment) ? currentDepartment : '';
}

function renderAssets() {
  $('assetCount').textContent = `${state.assets.length} assets`;
  $('assetGrid').innerHTML = state.assets.map(asset => {
    const key = assetKey(asset);
    const active = state.selected && assetKey(state.selected) === key ? ' active' : '';
    const thumb = asset.thumbnail_url
      ? `<img src="${esc(asset.thumbnail_url)}" alt="">`
      : `<span>${esc(asset.asset_code)}</span>`;
    return `<article class="asset-card${active}" data-key="${esc(key)}">
      <div class="thumb">${thumb}</div>
      <div class="asset-meta">
        <div class="asset-name"><span>${esc(asset.asset_code)}</span><span>${esc(asset.current || '-')}</span></div>
        <div class="asset-sub">${esc(asset.category)} / ${esc(asset.department)}</div>
        <div class="asset-sub">${asset.version_count} versions, latest ${esc(asset.latest || '-')}</div>
      </div>
    </article>`;
  }).join('');
  document.querySelectorAll('.asset-card').forEach(card => {
    card.addEventListener('click', () => {
      const asset = state.assets.find(item => assetKey(item) === card.dataset.key);
      if (asset) selectAsset(asset).catch(showError);
    });
  });
}

async function selectAsset(asset) {
  state.selected = asset;
  state.selectedVersion = asset.current || asset.latest || '';
  renderAssets();
  const params = {profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department};
  const data = await api(`/api/versions?${qs(params)}`);
  state.versions = data.versions;
  renderDetail(asset, data);
}

function renderDetail(asset, data) {
  $('detailEmpty').classList.add('hidden');
  $('detailPanel').classList.remove('hidden');
  $('detailTitle').textContent = asset.asset_code;
  $('detailSubtitle').textContent = asset.category;
  $('detailDepartment').textContent = asset.department;
  $('detailPreview').innerHTML = asset.thumbnail_url ? `<img src="${esc(asset.thumbnail_url)}" alt="">` : '<span>No thumbnail URL</span>';
  const options = data.versions.map(v => `<option value="${esc(v.version)}">${esc(v.version)}</option>`).join('');
  $('versionSelect').innerHTML = options;
  $('versionSelect').value = state.selectedVersion || data.current_status.current || data.current_status.latest || '';
  $('versionList').innerHTML = data.versions.map(v => {
    const marker = v.version === data.current_status.current ? 'Current' : '';
    return `<div class="version-row"><strong>${esc(v.version)}</strong><span>${v.file_count} files / ${v.total_bytes} bytes</span><span>${marker}</span></div>`;
  }).join('');
}

async function setCurrent() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  await api('/api/current', {method: 'PUT', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version})});
  await loadAssets();
  const refreshed = state.assets.find(item => assetKey(item) === assetKey(asset));
  if (refreshed) await selectAsset(refreshed);
}

async function resetCurrent() {
  const asset = state.selected;
  if (!asset) return;
  await api('/api/current', {method: 'PUT', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, reset: true})});
  await loadAssets();
  const refreshed = state.assets.find(item => assetKey(item) === assetKey(asset));
  if (refreshed) await selectAsset(refreshed);
}

async function pullToWorkspace() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  await api('/api/pull', {method: 'POST', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version, force: $('forcePull').checked})});
  status('Pulled to workspace');
}

async function copyThumbUrl() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  const url = await api(`/api/thumbnail-url?${qs({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version})}`);
  await navigator.clipboard.writeText(url);
  status('Thumbnail URL copied');
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
  await api('/api/thumbnails', {method: 'POST', body: form});
  await loadAssets();
  const refreshed = state.assets.find(item => assetKey(item) === assetKey(asset));
  if (refreshed) await selectAsset(refreshed);
}

function assetKey(asset) {
  return `${asset.category}/${asset.asset_code}/${asset.department}`;
}

function showError(error) {
  status(error.message || String(error));
}

$('authForm').addEventListener('submit', event => {
  event.preventDefault();
  state.token = $('tokenInput').value.trim();
  sessionStorage.setItem('adsToken', state.token);
  init().catch(showError);
});
$('profileSelect').addEventListener('change', () => { state.profile = $('profileSelect').value; loadAssets().catch(showError); });
$('searchInput').addEventListener('input', () => loadAssets().catch(showError));
$('categoryFilter').addEventListener('change', () => loadAssets().catch(showError));
$('departmentFilter').addEventListener('change', () => loadAssets().catch(showError));
$('refreshButton').addEventListener('click', () => loadAssets().catch(showError));
$('logoutButton').addEventListener('click', () => { sessionStorage.removeItem('adsToken'); location.reload(); });
$('setCurrentButton').addEventListener('click', () => setCurrent().catch(showError));
$('resetCurrentButton').addEventListener('click', () => resetCurrent().catch(showError));
$('pullButton').addEventListener('click', () => pullToWorkspace().catch(showError));
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
            Component::Normal(value) if value == ".git" || value == ".svn" || value == ".hg"
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
        version
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
        version
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
        assert!(VersionId::from_str("001").is_err());
        assert!(VersionId::from_str("v000").is_err());
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
        store
            .new_version_folder(&workspace, &department_key)
            .unwrap();
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
        store
            .new_version_folder(&workspace, &department_key)
            .unwrap();
        fs::write(
            version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd"),
            "v1",
        )
        .unwrap();
        store
            .add_version_folder(&workspace, &department_key, VersionId(1))
            .unwrap();
        store
            .new_version_folder(&workspace, &department_key)
            .unwrap();
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
        assert_eq!(assets["assets"][0]["current"], "v002");
        assert!(
            assets["assets"][0]["thumbnail_url"]
                .as_str()
                .unwrap()
                .starts_with("https://assets.example.com/objects/sha256/")
        );

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
        assert_eq!(set_current["current"], "v001");
        assert_eq!(set_current["explicit"], true);

        fs::remove_dir_all(version_folder(&workspace, &department_key, VersionId(1))).unwrap();
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
            fs::read_to_string(
                version_folder(&workspace, &department_key, VersionId(1)).join("crate.usd")
            )
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
        store
            .new_version_folder(&workspace, &department_key)
            .unwrap();
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

    fn test_web_state(store_path: &Path, workspace: &Path) -> Arc<WebState> {
        let profile = ServeProfile::new(
            "main".to_string(),
            store_path.to_path_buf(),
            workspace.to_path_buf(),
        )
        .unwrap();
        Arc::new(WebState::from(ServeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            auth_token: "secret".to_string(),
            profiles: BTreeMap::from([(profile.name.clone(), profile)]),
            max_upload_bytes: 10 * 1024 * 1024,
        }))
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
